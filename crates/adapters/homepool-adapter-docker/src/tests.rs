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
    };
    let unlabelled: Observation = serde_json::from_value(serde_json::json!({
        "id": id, "labels": null, "state": "created"
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
