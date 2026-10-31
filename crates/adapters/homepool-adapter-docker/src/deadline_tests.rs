//! Controlled expiry tests. The watchdog detects hangs; elapsed time and child
//! startup speed are never correctness signals. Procfs is used only to witness
//! the actual transport pipes and process identities before cancellation.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{fs::OpenOptionsExt, net::UnixListener},
    },
    path::PathBuf,
    process::Command,
    sync::mpsc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rustix::{
    fs::OFlags,
    io::Errno,
    process::{Pid, WaitId, WaitIdOptions, WaitOptions, waitid, waitpid},
};

use crate::{
    adapter::start_created, command::execute_with_remaining, endpoint::Endpoint, error::Error,
};

fn watchdog(test: impl FnOnce() + Send + 'static) {
    let (send, receive) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(test));
        let _ = send.send(result);
    });
    if let Err(panic) = receive
        .recv_timeout(Duration::from_secs(30))
        .expect("transport test watchdog expired")
    {
        std::panic::resume_unwind(panic);
    }
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "deadline-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn stalled(&self, exit_early: bool) -> Command {
        let mut command = Command::new("sh");
        // Publish readiness atomically after the descendant inherits the pipes.
        // For the early-exit case, the test opens witnesses before releasing the
        // leader, then waits for its exit before expiring the operation.
        command.args(["-c", if exit_early {
            "sleep 300 & printf '%s %s' $$ $! > \"$1/pending\"; mv \"$1/pending\" \"$1/ready\"; while [ ! -e \"$1/exit\" ]; do sleep 0.01; done; exit 0"
        } else {
            "echo dummy-secret >&2; sleep 300 & printf '%s %s' $$ $! > \"$1/pending\"; mv \"$1/pending\" \"$1/ready\"; wait"
        }, "transport-test"]);
        command.arg(&self.0);
        command
    }

    fn ready(&self) -> Option<Pipes> {
        let marker = self.0.join("ready");
        match fs::read_to_string(marker) {
            Ok(recorded) => Some(Pipes::open(&recorded)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => panic!("readiness: {error}"),
        }
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Pipes {
    leader: Pid,
    descendant: Option<String>,
    input: File,
    output: File,
}
impl Pipes {
    fn open(recorded: &str) -> Self {
        let mut pids = recorded.split_whitespace();
        let leader = Pid::from_raw(pids.next().unwrap().parse().unwrap()).unwrap();
        let base = format!("/proc/{}/fd", leader.as_raw_nonzero());
        let input = OpenOptions::new()
            .write(true)
            .custom_flags(OFlags::NONBLOCK.bits() as i32)
            .open(format!("{base}/0"))
            .unwrap();
        let output = OpenOptions::new()
            .read(true)
            .custom_flags(OFlags::NONBLOCK.bits() as i32)
            .open(format!("{base}/1"))
            .unwrap();
        Self {
            leader,
            descendant: pids.next().map(str::to_owned),
            input,
            output,
        }
    }

    fn assert_closed(mut self) {
        assert!(
            matches!(
                waitpid(Some(self.leader), WaitOptions::NOHANG),
                Err(Errno::CHILD)
            ),
            "CLI must be reaped"
        );
        if let Some(pid) = self.descendant {
            while let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
                if stat.rsplit_once(')').unwrap().1.starts_with(" Z") {
                    break;
                }
                std::thread::yield_now();
            }
        }
        drain_to_eof(&mut self.output);
        loop {
            match self.input.write(b"closed?") {
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => break,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => std::thread::yield_now(),
                Ok(_) => std::thread::yield_now(),
                other => panic!("stdin must lose every reader: {other:?}"),
            }
        }
    }
}

fn drain_to_eof(reader: &mut impl Read) {
    loop {
        match reader.read(&mut [0; 8192]) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => std::thread::yield_now(),
            Err(error) => panic!("pipe must close: {error}"),
        }
    }
}

#[test]
fn ready_transport_is_terminated_reaped_and_both_pipes_close() {
    watchdog(|| {
        for exit_early in [false, true] {
            let scratch = Scratch::new();
            let mut pipes = None;
            let error = execute_with_remaining(
                &mut scratch.stalled(exit_early),
                "create",
                &vec![b'x'; 1024 * 1024],
                || {
                    if pipes.is_none() {
                        pipes = scratch.ready();
                        if pipes.is_some() && exit_early {
                            fs::write(scratch.0.join("exit"), "").unwrap();
                        }
                    }
                    if let Some(pipes) = &pipes {
                        let exited = !exit_early
                            || waitid(
                                WaitId::Pid(pipes.leader),
                                WaitIdOptions::EXITED
                                    | WaitIdOptions::NOHANG
                                    | WaitIdOptions::NOWAIT,
                            )
                            .unwrap()
                            .is_some();
                        if exited {
                            return Duration::ZERO;
                        }
                    }
                    Duration::MAX
                },
            )
            .unwrap_err();
            assert!(matches!(error, Error::Deadline("create")));
            pipes.expect("ready before expiry").assert_closed();
        }
    });
}

fn recovery_timeout(timed_phase: &'static str) {
    let scratch = Scratch::new();
    let id = "a".repeat(64);
    let mut expired = false;
    let mut pipes = None;
    let mut phases = Vec::new();
    let error = start_created(&id, "sandbox-a", |operation, args| {
        assert_eq!(args.last().unwrap(), &id);
        phases.push(operation);
        let mut cli = if operation == timed_phase {
            scratch.stalled(false)
        } else {
            let mut failure = Command::new("sh");
            failure.args(["-c", "echo dummy-secret >&2; exit 19"]);
            failure
        };
        execute_with_remaining(&mut cli, operation, &[], || {
            if !expired && operation == timed_phase {
                pipes = scratch.ready();
                expired = pipes.is_some();
            }
            if expired {
                Duration::ZERO
            } else {
                Duration::MAX
            }
        })
    })
    .unwrap_err();
    assert_eq!(phases, ["start", "cleanup"]);
    pipes.expect("ready before expiry").assert_closed();
    let text = format!("{error} {error:?}");
    assert!(text.contains("deadline exceeded"));
    assert!(text.contains("outcome uncertain: true"));
    assert!(text.contains(&id));
    assert!(!text.contains("dummy-secret"));
    let Error::StartFailed {
        container_id,
        sandbox_id: _,
        start_error,
        cleanup_error,
        outcome_uncertain,
    } = error
    else {
        panic!("recovery error")
    };
    assert_eq!(container_id, id);
    assert!(outcome_uncertain);
    assert!(matches!(
        *cleanup_error.unwrap(),
        Error::Deadline("cleanup")
    ));
    if timed_phase == "start" {
        assert!(matches!(*start_error, Error::Deadline("start")));
    } else {
        assert!(matches!(
            *start_error,
            Error::Command {
                operation: "start",
                code: Some(19)
            }
        ));
    }
}

#[test]
fn timed_out_start_retains_deadline_and_known_id() {
    watchdog(|| recovery_timeout("start"));
}

#[test]
fn timed_out_rollback_retains_both_causes_and_known_id() {
    watchdog(|| recovery_timeout("cleanup"));
}

#[test]
#[ignore = "requires the Docker CLI and Unix sockets"]
fn stalled_engine_expires_after_connection_readiness() {
    watchdog(|| {
        let scratch = Scratch::new();
        // A procfs alias keeps long TMPDIR values within Unix socket limits.
        let directory = File::open(&scratch.0).unwrap();
        let socket = PathBuf::from(format!(
            "/proc/{}/fd/{}/engine.sock",
            std::process::id(),
            directory.as_raw_fd()
        ));
        let listener = UnixListener::bind(&socket).unwrap();
        let (send, receive) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            send.send(listener.accept().unwrap().0).unwrap();
        });
        let endpoint = Endpoint::unix(&socket).unwrap();
        let mut cli = Command::new("sh");
        // exec retains the recorded pid. Acceptance proves Docker has started;
        // a slow wrapper or scheduler merely postpones the readiness signal.
        cli.args([
            "-c",
            "printf '%s' $$ > \"$1/ready\"; shift; exec docker \"$@\"",
            "engine-test",
        ]);
        cli.arg(&scratch.0)
            .args(["--host", endpoint.as_str(), "info"]);
        for (key, _) in crate::command::command(&endpoint, &[]).get_envs() {
            cli.env_remove(key);
        }
        let mut connection = None;
        let mut pipes = None;
        let result = execute_with_remaining(&mut cli, "capacity", &[], || {
            if let Ok(stream) = receive.try_recv() {
                connection = Some(stream);
                pipes = scratch.ready();
                assert!(pipes.is_some());
            }
            if connection.is_some() {
                Duration::ZERO
            } else {
                Duration::MAX
            }
        });
        assert!(matches!(result, Err(Error::Deadline("capacity"))));
        pipes.unwrap().assert_closed();
        let mut connection = connection.unwrap();
        connection.set_nonblocking(true).unwrap();
        drain_to_eof(&mut connection);
    });
}

#[test]
fn cancellation_reaps_transport_and_closes_inherited_pipes() {
    watchdog(|| {
        use std::sync::atomic::{AtomicBool, Ordering};
        for (input, capture_stderr) in [
            (Vec::new(), false),
            (Vec::new(), true),
            (vec![b'x'; 1024 * 1024], false),
        ] {
            let scratch = Scratch::new();
            let cancelled = AtomicBool::new(false);
            let mut witnesses = None;
            let error = crate::command::execute_captured(
                &mut scratch.stalled(false),
                "create",
                &input,
                || {
                    if witnesses.is_none()
                        && let Some(pipes) = scratch.ready()
                    {
                        witnesses = Some(pipes);
                        cancelled.store(true, Ordering::Release);
                    }
                    Duration::from_secs(30)
                },
                &cancelled,
                capture_stderr,
                |_, _| {},
            )
            .unwrap_err();
            assert!(matches!(error, Error::Cancelled));
            witnesses.unwrap().assert_closed();
            let error = crate::command::execute_captured(
                &mut Command::new("no-such-command"),
                "create",
                &[],
                || Duration::from_secs(30),
                &cancelled,
                capture_stderr,
                |_, _| {},
            )
            .unwrap_err();
            assert!(matches!(error, Error::Cancelled));
        }
    });
}
