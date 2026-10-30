#![cfg(target_os = "linux")]

use std::{
    fs, io,
    os::unix::{
        fs::PermissionsExt,
        process::{CommandExt, ExitStatusExt},
    },
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use rustix::process::{Pid, Signal, kill_process_group};

struct Running {
    cli: Option<Child>,
    wrapper: Option<Pid>,
    scratch: PathBuf,
}

#[test]
fn signals_terminate_blocked_stdout_after_the_probe_is_reaped() {
    for (signal, number) in [(Signal::INT, 2), (Signal::TERM, 15)] {
        let scratch = PathBuf::from(
            std::env::var_os("TMPDIR").unwrap_or_else(|| env!("CARGO_TARGET_TMPDIR").into()),
        )
        .join(format!(
            "garage-output-signals-{}-{number}",
            std::process::id()
        ));
        fs::create_dir_all(scratch.join("bin")).unwrap();
        let mut running = Running {
            cli: None,
            wrapper: None,
            scratch,
        };
        let pids = running.scratch.join("pids");
        let wrapper = running.scratch.join("bin/garage");
        fs::write(
            &wrapper,
            r#"#!/bin/sh
cat >/dev/null
printf '%s' $$ > "$HOMEPOOL_TEST_PIDS"
printf '[]'
"#,
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        let (reader, writer) = io::pipe().unwrap();
        let capacity = rustix::pipe::fcntl_getpipe_size(&reader).unwrap();
        let mut members = String::from("version = 1\n");
        // Every JSON member occupies more than 64 bytes, so stdout must stall.
        for id in 0..=capacity / 64 {
            members.push_str(&format!(
                "[[members]]\nid = {id}\ntier = 'core'\nstate = 'active'\ncapabilities = {{}}\n"
            ));
        }
        fs::write(running.scratch.join("members.toml"), members).unwrap();
        let mut paths = vec![running.scratch.join("bin")];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        running.cli = Some(
            Command::new(env!("CARGO_BIN_EXE_homepool"))
                .args([
                    "status",
                    "--config",
                    "members.toml",
                    "--garage-config",
                    "private-garage.toml",
                    "--json",
                ])
                .current_dir(&running.scratch)
                .env("PATH", std::env::join_paths(paths).unwrap())
                .env("HOMEPOOL_TEST_PIDS", &pids)
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(writer)
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let cli = running.cli.as_mut().unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while rustix::io::ioctl_fionread(&reader).unwrap() < capacity as u64 {
            assert!(
                cli.try_wait().unwrap().is_none(),
                "CLI exited before filling stdout"
            );
            assert!(Instant::now() < until, "stdout did not fill");
            thread::sleep(Duration::from_millis(5));
        }
        let wrapper_id: i32 = fs::read_to_string(&pids).unwrap().parse().unwrap();
        assert!(
            !PathBuf::from(format!("/proc/{wrapper_id}")).exists(),
            "probe must be reaped before output blocks"
        );
        assert!(cli.try_wait().unwrap().is_none());
        kill_process_group(Pid::from_raw(cli.id() as i32).unwrap(), signal).unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        while cli.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < until,
                "blocked output ignored the termination signal"
            );
            thread::sleep(Duration::from_millis(5));
        }
        // Keep the full pipe's reader open and undrained until after termination.
        let output = running.cli.take().unwrap().wait_with_output().unwrap();
        assert_eq!(output.status.signal(), Some(number));
        assert_eq!(output.status.code(), None);
        assert!(output.stderr.is_empty());
        drop(reader);
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // Also clean up when exercising an implementation that leaves the wrapper.
        if let Some(wrapper) = self.wrapper {
            let _ = kill_process_group(wrapper, Signal::KILL);
        }
        if let Some(cli) = &mut self.cli {
            let _ = cli.kill();
            let _ = cli.wait();
        }
        let _ = fs::remove_dir_all(&self.scratch);
    }
}

#[test]
fn interrupting_live_status_stops_and_reaps_the_wrapper_and_stops_descendants() {
    for (signal, code) in [(Signal::INT, 130), (Signal::TERM, 143)] {
        let scratch = PathBuf::from(
            std::env::var_os("TMPDIR").unwrap_or_else(|| env!("CARGO_TARGET_TMPDIR").into()),
        )
        .join(format!("garage-signals-{}-{code}", std::process::id()));
        fs::create_dir_all(scratch.join("bin")).unwrap();
        let mut running = Running {
            cli: None,
            wrapper: None,
            scratch,
        };
        let pids = running.scratch.join("pids");
        let continued = running.scratch.join("continued");
        let wrapper = running.scratch.join("bin/garage");
        fs::write(
            &wrapper,
            r#"#!/bin/sh
cat >/dev/null
(sleep 1; printf continued > "$HOMEPOOL_TEST_CONTINUED"; exec sleep 30) &
printf '%s %s' $$ $! > "$HOMEPOOL_TEST_PIDS"
wait
"#,
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            running.scratch.join("members.toml"),
            "version = 1\nmembers = []",
        )
        .unwrap();
        let mut paths = vec![running.scratch.join("bin")];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        running.cli = Some(
            Command::new(env!("CARGO_BIN_EXE_homepool"))
                .args([
                    "status",
                    "--config",
                    "members.toml",
                    "--garage-config",
                    "private-garage.toml",
                    "--json",
                ])
                .current_dir(&running.scratch)
                .env("PATH", std::env::join_paths(paths).unwrap())
                .env("HOMEPOOL_TEST_PIDS", &pids)
                .env("HOMEPOOL_TEST_CONTINUED", &continued)
                // Simulate the CLI's terminal foreground group without signaling the test runner.
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let until = Instant::now() + Duration::from_secs(5);
        let descendant = loop {
            if let Ok(recorded) = fs::read_to_string(&pids) {
                let ids: Vec<_> = recorded
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                if let [wrapper, descendant] = ids.as_slice() {
                    running.wrapper = Pid::from_raw(*wrapper);
                    break *descendant;
                }
            }
            assert!(Instant::now() < until, "wrapper did not start");
            thread::sleep(Duration::from_millis(5));
        };
        let cli = running.cli.as_mut().unwrap();
        kill_process_group(Pid::from_raw(cli.id() as i32).unwrap(), signal).unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        while cli.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < until,
                "interrupted CLI did not finish cleanup"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let output = running.cli.take().unwrap().wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(code));
        assert!(output.stdout.is_empty());
        assert_eq!(
            output.stderr,
            b"homepool: Garage operation cancelled; inspect privately before retrying\n"
        );
        assert!(
            !PathBuf::from(format!(
                "/proc/{}",
                running.wrapper.unwrap().as_raw_nonzero()
            ))
            .exists(),
            "wrapper must be reaped before the CLI exits"
        );
        let until = Instant::now() + Duration::from_secs(2);
        while let Ok(stat) = fs::read_to_string(format!("/proc/{descendant}/stat")) {
            if stat.rsplit_once(')').unwrap().1.starts_with(" Z") {
                break;
            }
            assert!(
                Instant::now() < until,
                "wrapper descendant survived cancellation"
            );
            thread::sleep(Duration::from_millis(5));
        }
        running.wrapper = None;
        thread::sleep(Duration::from_millis(1100));
        assert!(
            !continued.exists(),
            "wrapper continued work after interruption"
        );
    }
}
