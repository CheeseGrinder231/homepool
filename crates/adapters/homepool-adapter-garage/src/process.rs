use std::{
    ffi::OsString,
    io::{self, Read, Write},
    os::{fd::AsFd, unix::process::CommandExt},
    process::{Child, Command, Stdio},
    time::Instant,
};

use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid},
};
use serde_json::Value;

use super::{error::Error, settings::POLL_INTERVAL};

// Retain the group leader until cleanup so its numeric pid cannot be reused
// before the group is killed. This also cancels wrappers retaining pipe handles.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(group) = Pid::from_raw(self.0.id() as i32) {
            let _ = kill_process_group(group, Signal::KILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(super) fn call(
    executable: &OsString,
    prefix: &[OsString],
    operation: &str,
    request: &Value,
    deadline: Instant,
) -> Result<Value, Error> {
    let input =
        serde_json::to_vec(request).map_err(|_| Error("could not encode Garage request"))?;
    check_deadline(deadline)?;
    let mut running = Running(
        Command::new(executable)
            .args(prefix)
            .args(["json-api", operation, "-"])
            .env("RUST_LOG", "off")
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| Error("could not start Garage; check installation and private config"))?,
    );
    let mut input_pipe = running.0.stdin.take();
    let mut output_pipe = running.0.stdout.take().expect("piped stdout");
    for pipe in [
        input_pipe.as_ref().expect("piped stdin").as_fd(),
        output_pipe.as_fd(),
    ] {
        let flags = fcntl_getfl(pipe).map_err(|_| Error("could not configure Garage pipe"))?;
        fcntl_setfl(pipe, flags | OFlags::NONBLOCK)
            .map_err(|_| Error("could not configure Garage pipe"))?;
    }
    let mut sent = 0;
    let mut output = Vec::new();
    let mut eof = false;
    loop {
        check_deadline(deadline)?;
        let mut progressed = false;
        if sent == input.len() {
            input_pipe.take();
        }
        if let Some(pipe) = input_pipe.as_mut() {
            match pipe.write(&input[sent..]) {
                Ok(0) => return Err(Error("could not send Garage request")),
                Ok(count) => {
                    sent += count;
                    progressed = true;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(Error("could not send Garage request")),
            }
        }
        if !eof {
            let mut buffer = [0; 8192];
            match output_pipe.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(count) => {
                    output.extend_from_slice(&buffer[..count]);
                    progressed = true;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(Error("could not read Garage response")),
            }
        }
        // Observe exit without reaping; Drop kills descendants before reaping.
        let pid = Pid::from_raw(running.0.id() as i32).expect("child pid");
        if let Some(status) = waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )
        .map_err(|_| Error("could not wait for Garage"))?
            && eof
            && input_pipe.is_none()
        {
            check_deadline(deadline)?;
            if status.exit_status() != Some(0) {
                return Err(Error(
                    "Garage operation failed; inspect the installation privately",
                ));
            }
            let parsed =
                serde_json::from_slice(&output).map_err(|_| Error("invalid Garage JSON response"));
            check_deadline(deadline)?;
            return parsed;
        }
        if !progressed {
            std::thread::sleep(
                POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
}

pub(super) fn check_deadline(deadline: Instant) -> Result<(), Error> {
    if Instant::now() >= deadline {
        Err(Error::DEADLINE)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use rustix::{
        io::Errno,
        process::{Pid, WaitOptions, waitpid},
    };
    use serde_json::{Value, json};
    use std::{
        fs, process,
        time::{Duration, Instant},
    };

    use super::{Error, call};

    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn deadline_covers_blocked_input_output_and_completion_and_reaps_the_child() {
        let scratch =
            Scratch(std::env::temp_dir().join(format!("garage-deadline-{}", process::id())));
        fs::create_dir(&scratch.0).unwrap();
        let marker = scratch.0.join("pids");
        for (behavior, large_input) in [
            ("sleep 30 & printf '%s %s' $$ $! > \"$1\"; wait", true),
            (
                "cat >/dev/null; sleep 30 & printf '%s %s' $$ $! > \"$1\"; wait",
                true,
            ),
            ("sleep 30 & printf '%s %s' $$ $! > \"$1\"; exit 0", false),
            (
                "cat >/dev/null; exec 1>&-; sleep 30 & printf '%s %s' $$ $! > \"$1\"; wait",
                false,
            ),
        ] {
            let request = if large_input {
                json!({"data": "x".repeat(1024 * 1024)})
            } else {
                Value::Null
            };
            let started = Instant::now();
            let error = call(
                &"sh".into(),
                &[
                    "-c".into(),
                    behavior.into(),
                    "deadline-test".into(),
                    marker.as_os_str().into(),
                ],
                "ApplyClusterLayout",
                &request,
                started + Duration::from_millis(500),
            )
            .unwrap_err();
            assert_eq!(error, Error::DEADLINE);
            assert!(started.elapsed() < Duration::from_secs(3));
            let recorded = fs::read_to_string(&marker).unwrap();
            let pids: Vec<_> = recorded.split_whitespace().collect();
            let child = Pid::from_raw(pids[0].parse().unwrap()).unwrap();
            assert!(
                matches!(waitpid(Some(child), WaitOptions::NOHANG), Err(Errno::CHILD)),
                "direct child must be reaped"
            );
            #[cfg(target_os = "linux")]
            {
                let until = Instant::now() + Duration::from_secs(2);
                while let Ok(stat) = fs::read_to_string(format!("/proc/{}/stat", pids[1])) {
                    if stat.rsplit_once(')').unwrap().1.starts_with(" Z") {
                        break;
                    }
                    assert!(
                        Instant::now() < until,
                        "wrapper descendant survived cancellation"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    #[test]
    fn pipes_progress_concurrently_and_expired_work_never_starts() {
        let request = json!({"data": "x".repeat(256 * 1024)});
        let script = r#"printf '{"padding":"'; head -c 131072 /dev/zero | tr '\000' x; printf '"}'; cat >/dev/null"#;
        let response = call(
            &"sh".into(),
            &["-c".into(), script.into()],
            "test",
            &request,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(response["padding"].as_str().unwrap().len(), 131072);
        assert_eq!(
            call(
                &"no-such-program".into(),
                &[],
                "test",
                &Value::Null,
                Instant::now()
            )
            .unwrap_err(),
            Error::DEADLINE
        );
    }
}
