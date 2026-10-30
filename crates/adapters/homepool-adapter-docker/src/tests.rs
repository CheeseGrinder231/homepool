use std::{collections::BTreeMap, path::Path};

use super::*;
use crate::{
    adapter::{Observation, owned_observation, validate_id},
    command,
    settings::{MANAGED_LABEL, MEMBER_LABEL, SANDBOX_LABEL},
    spec,
};
use homepool_core::MemberId;

fn spec() -> SandboxSpec {
    SandboxSpec {
        sandbox_id: "sandbox-a".into(),
        image: "alpine:3.22".into(),
        cpu_millicores: 1250,
        memory_bytes: 64 * 1024 * 1024,
        mounts: vec![],
        environment: Environment::default(),
        command: vec!["sleep".into(), "60".into()],
    }
}

#[test]
fn local_and_ssh_endpoints_are_explicit() {
    assert_eq!(
        Endpoint::unix("/run/docker.sock").unwrap().as_str(),
        "unix:///run/docker.sock"
    );
    assert_eq!(
        Endpoint::unix("/run/member/docker.sock").unwrap().as_str(),
        "unix:///run/member/docker.sock"
    );
    assert_eq!(
        Endpoint::unix("/run/socket space%.sock").unwrap().as_str(),
        "unix:///run/socket space%.sock"
    );
    assert_eq!(
        Endpoint::ssh("192.0.2.1", None, None).unwrap().as_str(),
        "ssh://192.0.2.1"
    );
    assert_eq!(
        Endpoint::ssh(
            "member-alias",
            Some(2222),
            Some(Path::new("/run/member/docker.sock"))
        )
        .unwrap()
        .as_str(),
        "ssh://member-alias:2222/run/member/docker.sock"
    );
    assert_eq!(
        Endpoint::ssh("2001:db8::1", Some(22), None)
            .unwrap()
            .as_str(),
        "ssh://[2001:db8::1]:22"
    );
}

#[test]
fn endpoints_reject_credentials_options_and_invalid_paths() {
    for host in [
        "",
        "-oProxyCommand=command",
        "member@192.0.2.1",
        "member:password@192.0.2.1",
        "192.0.2.1/path",
        "member\nname",
        "[2001:db8::1]",
    ] {
        assert!(Endpoint::ssh(host, None, None).is_err());
    }
    assert!(Endpoint::ssh("192.0.2.1", Some(0), None).is_err());
    assert!(Endpoint::ssh("192.0.2.1", None, Some(Path::new("relative.sock"))).is_err());
    for socket in [
        "/run/socket space.sock",
        "/run/socket;command",
        "/run/$(command)",
        "/run/socket%20.sock",
        "/run/socket'quote",
    ] {
        assert!(Endpoint::ssh("192.0.2.1", None, Some(Path::new(socket))).is_err());
    }
    for socket in [
        "",
        "relative.sock",
        "/run/socket\0",
        "/run/socket ",
        "/run/unix://socket",
    ] {
        assert!(Endpoint::unix(socket).is_err());
    }
}

#[test]
fn secrets_only_enter_stdin_and_debug_is_redacted() {
    let mut spec = spec();
    let secret = "dummy-secret = 'quoted' $literal ";
    spec.environment
        .insert("REGISTRATION_TOKEN", secret)
        .unwrap();
    spec.environment.insert("EMPTY", "").unwrap();
    let args = spec::create_args(MemberId(7), &spec).unwrap();
    assert!(!args.join(" ").contains(secret));
    assert!(!args.join(" ").contains("REGISTRATION_TOKEN"));
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--env-file", "/dev/stdin"])
    );
    assert!(args.windows(2).any(|pair| pair == ["--cpus", "1.250"]));
    assert!(args.contains(&"homepool.member=7".to_owned()));
    assert!(args.contains(&"homepool.sandbox=sandbox-a".to_owned()));
    assert_eq!(
        spec.environment.stdin_bytes(),
        format!("EMPTY=\nREGISTRATION_TOKEN={secret}\n").as_bytes()
    );
    assert!(!format!("{spec:?}").contains(secret));
    let endpoint = Endpoint::unix("/run/docker.sock").unwrap();
    let command = command::command(&endpoint, &args);
    let actual: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_str().unwrap())
        .collect();
    assert_eq!(&actual[..2], ["--host", "unix:///run/docker.sock"]);
    assert!(!format!("{command:?}").contains(secret));
    assert!(
        command
            .get_envs()
            .any(|(key, value)| key == "DOCKER_CONTEXT" && value.is_none())
    );
}

#[test]
fn env_file_input_cannot_inject_entries_or_inherit_values() {
    let mut env = Environment::default();
    for (key, value) in [
        ("TOKEN", "secret\nOTHER=changed"),
        ("TOKEN", "secret\r"),
        ("TOKEN", "secret\0"),
        ("#TOKEN", "secret"),
        ("KEY=bad", "secret"),
        ("", "secret"),
        ("1KEY", "secret"),
        (" KEY", "secret"),
    ] {
        let error = env.insert(key, value).unwrap_err();
        assert!(!format!("{error:?} {error}").contains("secret"));
    }
    assert!(env.insert("TOKEN", &"x".repeat(65_535)).is_err());
    assert_eq!(env.stdin_bytes(), b"");
    env.insert("_KEY_1", "a=b # literal").unwrap();
    assert_eq!(env.stdin_bytes(), b"_KEY_1=a=b # literal\n");
}

#[test]
fn invalid_specs_are_rejected_before_running_docker() {
    let docker = Docker::new(Endpoint::unix("/run/unused.sock").unwrap(), MemberId(7));
    let mut request = spec();
    request.cpu_millicores = 0;
    assert!(matches!(
        docker.start(&request),
        Err(Error::InvalidInput(_))
    ));
    request.cpu_millicores = u64::MAX;
    assert!(spec::create_args(MemberId(7), &request).is_err());
    request.cpu_millicores = 500;
    request.memory_bytes = 0;
    assert!(spec::create_args(MemberId(7), &request).is_err());
    request.memory_bytes = 64 * 1024 * 1024;
    request.sandbox_id = "bad\nlabel=value".into();
    assert!(spec::create_args(MemberId(7), &request).is_err());
    request.sandbox_id = "sandbox-a".into();
    request.image = "--privileged".into();
    assert!(spec::create_args(MemberId(7), &request).is_err());
}

#[test]
fn mounts_are_single_arguments_and_ambiguous_csv_is_rejected() {
    let mut request = spec();
    request.mounts.push(BindMount {
        source: "/data/source space".into(),
        target: "/data/input".into(),
        read_only: true,
    });
    let args = spec::create_args(MemberId(7), &request).unwrap();
    assert!(args.contains(&"type=bind,src=/data/source space,dst=/data/input,readonly".to_owned()));
    for source in ["relative", "/data,readonly", "/data\"quote", "/data\nline"] {
        request.mounts[0].source = source.into();
        assert!(spec::create_args(MemberId(7), &request).is_err());
    }
}

#[test]
fn ownership_requires_all_three_labels_and_exact_member_and_id() {
    let id = "a".repeat(64);
    let labels = BTreeMap::from([
        (MANAGED_LABEL.into(), "true".into()),
        (MEMBER_LABEL.into(), "7".into()),
        (SANDBOX_LABEL.into(), "sandbox-a".into()),
    ]);
    let observation = |labels| Observation {
        id: id.clone(),
        labels: Some(labels),
        state: "running".into(),
        limits: crate::limits::Limits {
            nano_cpus: 1_250_000_000,
            cpu_quota: 0,
            cpu_period: 0,
            memory_bytes: 64 * 1024 * 1024,
        },
    };
    let unlabelled: Observation = serde_json::from_value(serde_json::json!({
        "id": id, "labels": null, "state": "created",
        "limits": {"nano_cpus": 0, "cpu_quota": 0, "cpu_period": 0, "memory_bytes": 0}
    }))
    .unwrap();
    assert!(matches!(
        owned_observation(MemberId(7), &id, unlabelled),
        Err(Error::NotManaged)
    ));
    assert_eq!(
        owned_observation(MemberId(7), &id, observation(labels.clone()))
            .unwrap()
            .sandbox_id,
        "sandbox-a"
    );
    assert!(matches!(
        owned_observation(MemberId(8), &id, observation(labels.clone())),
        Err(Error::NotManaged)
    ));
    for key in [MANAGED_LABEL, MEMBER_LABEL, SANDBOX_LABEL] {
        let mut partial = labels.clone();
        partial.remove(key);
        assert!(matches!(
            owned_observation(MemberId(7), &id, observation(partial)),
            Err(Error::NotManaged)
        ));
    }
    assert!(matches!(
        owned_observation(MemberId(7), &"b".repeat(64), observation(labels)),
        Err(Error::Protocol(_))
    ));
    for invalid in ["--all", "", "short"] {
        assert!(validate_id(invalid).is_err());
    }
}

#[test]
fn inventory_limits_round_up_and_unbounded_is_not_zero() {
    use crate::limits::Limits;
    for (nano_cpus, cpu_quota, cpu_period, expected) in [
        (1_250_000_000, 0, 0, Some(1250)),
        (500_000_001, 0, 0, Some(501)),
        (0, 0, 0, None),
        (0, -1, 100_000, None),
        (0, 25_001, 100_000, Some(251)),
        (2_000_000_000, 50_000, 100_000, Some(500)),
    ] {
        let limits = Limits {
            nano_cpus,
            cpu_quota,
            cpu_period,
            memory_bytes: 0,
        };
        assert_eq!(limits.cpu_millicores().unwrap(), expected);
    }
    for (cpu_quota, cpu_period) in [(1, 0), (-2, 100_000), (i64::MAX, 1)] {
        assert!(
            Limits {
                nano_cpus: 0,
                cpu_quota,
                cpu_period,
                memory_bytes: 0
            }
            .cpu_millicores()
            .is_err()
        );
    }
    let id = "a".repeat(64);
    let observation = |nano_cpus, memory_bytes| {
        serde_json::from_value(serde_json::json!({
        "id": id, "state": "running",
        "labels": {"homepool.managed": "true", "homepool.member": "7", "homepool.sandbox": "sandbox-a"},
        "limits": {"nano_cpus": nano_cpus, "cpu_quota": 0, "cpu_period": 0, "memory_bytes": memory_bytes}
    })).unwrap()
    };
    let sized =
        owned_observation(MemberId(7), &id, observation(1_250_000_000, 67_108_864)).unwrap();
    assert_eq!(sized.cpu_millicores, Some(1250));
    assert_eq!(sized.memory_bytes, Some(67_108_864));
    let unbounded = owned_observation(MemberId(7), &id, observation(0, 0)).unwrap();
    assert_eq!(unbounded.cpu_millicores, None);
    assert_eq!(unbounded.memory_bytes, None);
    assert!(
        serde_json::from_value::<Observation>(
            serde_json::json!({"id": id, "state": "running", "labels": {}})
        )
        .is_err()
    );
}

#[test]
fn deadlines_reject_invalid_configuration_and_expired_work() {
    use std::{
        process::Command,
        time::{Duration, Instant},
    };
    let docker = Docker::new(Endpoint::unix("/run/unused.sock").unwrap(), MemberId(7));
    assert!(
        docker
            .clone()
            .with_operation_timeout(Duration::ZERO)
            .is_err()
    );
    assert!(docker.with_operation_timeout(Duration::MAX).is_err());
    // An expired operation must not even launch the transport.
    assert!(matches!(
        command::execute(
            &mut Command::new("no-such-program"),
            "test",
            &[],
            Instant::now()
        ),
        Err(Error::Deadline("test"))
    ));
}

#[test]
fn transport_streams_both_pipes_and_redacts_failures() {
    use std::{
        process::Command,
        time::{Duration, Instant},
    };
    let input = vec![b'x'; 256 * 1024];
    let mut echo = Command::new("sh");
    echo.args(["-c", "head -c 131072 /dev/zero; cat"]);
    let output = command::execute(
        &mut echo,
        "test",
        &input,
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(&output[..131072], vec![0; 131072]);
    assert_eq!(&output[131072..], input);
    let mut failure = Command::new("sh");
    failure.args(["-c", "echo dummy-secret >&2; exit 19"]);
    let error = command::execute(
        &mut failure,
        "test",
        &input,
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap_err();
    assert!(matches!(error, Error::Command { code: Some(19), .. }));
    assert!(!format!("{error:?} {error}").contains("dummy-secret"));
}

#[test]
fn stalled_transport_reaps_child_and_does_not_block_on_stdin() {
    use rustix::{
        io::Errno,
        process::{Pid, WaitOptions, waitpid},
    };
    use std::{
        fs,
        process::Command,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };
    let scratch = std::env::temp_dir().join(format!(
        "transport-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&scratch).unwrap();
    for finish in ["wait", "exit 0"] {
        let marker = scratch.join("pid");
        let mut stalled = Command::new("sh");
        // A descendant holds output open, including after an early CLI exit.
        // Neither process consumes the large stdin payload.
        stalled.args([
            "-c",
            &format!("sleep 30 & printf '%s %s' $$ $! > \"$1\"; {finish}"),
            "transport-test",
        ]);
        stalled.arg(&marker);
        let started = Instant::now();
        let error = command::execute(
            &mut stalled,
            "create",
            &vec![b'x'; 1024 * 1024],
            started + Duration::from_millis(500),
        )
        .unwrap_err();
        assert!(matches!(error, Error::Deadline("create")));
        assert!(started.elapsed() < Duration::from_secs(5));
        let recorded = fs::read_to_string(&marker).unwrap();
        let mut pids = recorded.split_whitespace();
        let pid = Pid::from_raw(pids.next().unwrap().parse().unwrap()).unwrap();
        // Reaped, rather than merely killed and left a zombie.
        assert!(matches!(
            waitpid(Some(pid), WaitOptions::NOHANG),
            Err(Errno::CHILD)
        ));
        #[cfg(target_os = "linux")]
        {
            let descendant = pids.next().unwrap();
            let until = Instant::now() + Duration::from_secs(2);
            // The descendant may briefly be a zombie waiting for its reaper.
            // It must no longer be running or holding transport descriptors.
            while let Ok(stat) = fs::read_to_string(format!("/proc/{descendant}/stat")) {
                if stat.rsplit_once(')').unwrap().1.starts_with(" Z") {
                    break;
                }
                assert!(
                    Instant::now() < until,
                    "transport descendant survived cancellation"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    fs::remove_dir_all(scratch).unwrap();
}
