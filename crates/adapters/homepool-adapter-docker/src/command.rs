use std::{
    io::{self, Read, Write},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid},
};

use crate::{
    endpoint::Endpoint,
    error::Error,
    settings::{DOCKER_PROGRAM, TRANSPORT_POLL_INTERVAL},
};

pub(crate) fn command(endpoint: &Endpoint, args: &[String]) -> Command {
    let mut command = Command::new(DOCKER_PROGRAM);
    command.arg("--host").arg(endpoint.as_str()).args(args);
    // Explicit endpoints must not inherit context or TLS endpoint overrides.
    for key in [
        "DOCKER_HOST",
        "DOCKER_CONTEXT",
        "DOCKER_TLS",
        "DOCKER_TLS_VERIFY",
        "DOCKER_CERT_PATH",
    ] {
        command.env_remove(key);
    }
    command
}

pub(crate) fn run(
    endpoint: &Endpoint,
    operation: &'static str,
    args: &[String],
    input: &[u8],
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>, Error> {
    execute_poll(
        &mut command(endpoint, args),
        operation,
        input,
        || deadline.saturating_duration_since(Instant::now()),
        cancelled,
    )
}

// Own the CLI and its SSH descendants as one transport. Every return path kills
// the group and reaps the direct child, including pipe/setup errors and unwinding.
struct Transport(Child);

impl Drop for Transport {
    fn drop(&mut self) {
        if let Some(group) = Pid::from_raw(self.0.id() as i32) {
            let _ = kill_process_group(group, Signal::KILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(test)]
pub(crate) fn execute(
    command: &mut Command,
    operation: &'static str,
    input: &[u8],
    deadline: Instant,
) -> Result<Vec<u8>, Error> {
    execute_with_remaining(command, operation, input, || {
        deadline.saturating_duration_since(Instant::now())
    })
}

// Private clock seam: production derives remaining time from the single public
// operation deadline. Tests can expire it after controlled transport readiness.
#[cfg(test)]
pub(crate) fn execute_with_remaining(
    command: &mut Command,
    operation: &'static str,
    input: &[u8],
    remaining: impl FnMut() -> Duration,
) -> Result<Vec<u8>, Error> {
    execute_poll(
        command,
        operation,
        input,
        remaining,
        &AtomicBool::new(false),
    )
}

pub(crate) fn execute_poll(
    command: &mut Command,
    operation: &'static str,
    input: &[u8],
    remaining: impl FnMut() -> Duration,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>, Error> {
    let mut output = Vec::new();
    execute_captured(
        command,
        operation,
        input,
        remaining,
        cancelled,
        false,
        |_, bytes| {
            output.extend_from_slice(bytes);
        },
    )?;
    Ok(output)
}

/// Log output needs both pipes; errors still expose only fixed operation names.
pub(crate) fn capture(
    endpoint: &Endpoint,
    args: &[String],
    deadline: Instant,
    cancelled: &AtomicBool,
    sink: impl FnMut(bool, &[u8]),
) -> Result<(), Error> {
    execute_captured(
        &mut command(endpoint, args),
        "logs",
        &[],
        || deadline.saturating_duration_since(Instant::now()),
        cancelled,
        true,
        sink,
    )
}

pub(crate) fn execute_captured(
    command: &mut Command,
    operation: &'static str,
    input: &[u8],
    mut remaining: impl FnMut() -> Duration,
    cancelled: &AtomicBool,
    capture_stderr: bool,
    mut sink: impl FnMut(bool, &[u8]),
) -> Result<(), Error> {
    if cancelled.load(Ordering::Acquire) {
        return Err(Error::Cancelled);
    }
    if remaining().is_zero() {
        return Err(Error::Deadline(operation));
    }
    let mut transport = Transport(
        command
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Ordinary commands discard diagnostics. Logs capture both pipes;
            // callers discard all captured output when the command fails.
            .stderr(if capture_stderr {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .spawn()
            .map_err(|_| Error::Io(operation))?,
    );
    let mut stdin = transport.0.stdin.take();
    let mut pipes = [
        transport
            .0
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn ReadPipe>),
        transport
            .0
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn ReadPipe>),
    ];
    nonblocking(stdin.as_ref().expect("piped stdin")).map_err(|_| Error::Io(operation))?;
    for pipe in pipes.iter().flatten() {
        nonblocking(pipe).map_err(|_| Error::Io(operation))?;
    }
    let mut offset = 0;

    let mut input_failed = false;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        let time_left = remaining();
        if time_left.is_zero() {
            return Err(Error::Deadline(operation));
        }
        let mut progressed = false;
        if offset == input.len() {
            // EOF is required even for empty input.
            stdin.take();
        }
        if let Some(pipe) = stdin.as_mut() {
            match pipe.write(&input[offset..]) {
                Ok(0) => {
                    input_failed = true;
                    stdin.take();
                }
                Ok(count) => {
                    offset += count;
                    progressed = true;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    input_failed = true;
                    stdin.take();
                }
            }
        }
        for (index, slot) in pipes.iter_mut().enumerate() {
            let Some(pipe) = slot.as_mut() else { continue };
            let mut buffer = [0; 8192];
            match pipe.read(&mut buffer) {
                Ok(0) => {
                    slot.take();
                }
                Ok(count) => {
                    sink(index == 1, &buffer[..count]);
                    progressed = true;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(Error::Io(operation)),
            }
        }
        // Keep the leader waitable until Drop kills its group. Reaping it here
        // could let its numeric pid/group id be reused before group cleanup.
        let pid = Pid::from_raw(transport.0.id() as i32).expect("child pid");
        if let Some(status) = waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )
        .map_err(|_| Error::Io(operation))?
            && pipes.iter().all(Option::is_none)
            && stdin.is_none()
        {
            if cancelled.load(Ordering::Acquire) {
                return Err(Error::Cancelled);
            }
            if status.exit_status() != Some(0) {
                return Err(Error::Command {
                    operation,
                    code: status.exit_status(),
                });
            }
            if input_failed {
                return Err(Error::Io(operation));
            }
            return Ok(());
        }
        if !progressed {
            std::thread::sleep(TRANSPORT_POLL_INTERVAL.min(time_left));
        }
    }
}

trait ReadPipe: Read + std::os::fd::AsFd {}
impl<T: Read + std::os::fd::AsFd> ReadPipe for T {}

fn nonblocking(fd: &(impl std::os::fd::AsFd + ?Sized)) -> io::Result<()> {
    let flags = fcntl_getfl(fd)?;
    fcntl_setfl(fd, flags | OFlags::NONBLOCK)?;
    Ok(())
}
