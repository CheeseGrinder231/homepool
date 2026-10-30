//! Opt-in tests requiring a local Unix Docker engine and alpine:3.22.
//! CI runs these explicitly; an unavailable engine is a failure, never a skip.

use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use homepool_adapter_docker::{BindMount, Docker, Endpoint, Environment, Error, SandboxSpec};
use homepool_core::MemberId;
use serde_json::Value;

fn endpoint() -> Endpoint {
    Endpoint::unix(
        std::env::var_os("HOMEPOOL_TEST_DOCKER_SOCKET")
            .unwrap_or_else(|| "/var/run/docker.sock".into()),
    )
    .unwrap()
}

fn engine() -> Docker {
    Docker::new(endpoint(), MemberId(4))
}

fn unique() -> String {
    format!(
        "test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn spec() -> SandboxSpec {
    SandboxSpec {
        sandbox_id: unique(),
        image: "alpine:3.22".into(),
        cpu_millicores: 500,
        memory_bytes: 64 * 1024 * 1024,
        mounts: vec![],
        environment: Environment::default(),
        // Handle stop immediately, without waiting for the stop timeout.
        command: vec![
            "sh".into(),
            "-c".into(),
            "trap 'exit 0' TERM; while :; do sleep 1 & wait $!; done".into(),
        ],
    }
}

fn cli(args: &[&str]) -> Vec<u8> {
    let output = Command::new("docker")
        .arg("--host")
        .arg(endpoint().as_str())
        .env_remove("DOCKER_CONTEXT")
        .env_remove("DOCKER_HOST")
        .env_remove("DOCKER_TLS")
        .env_remove("DOCKER_TLS_VERIFY")
        .env_remove("DOCKER_CERT_PATH")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "test Docker command failed; diagnostics redacted"
    );
    output.stdout
}

struct Container(String);

impl Drop for Container {
    fn drop(&mut self) {
        // Best-effort cleanup even when an assertion panics.
        let _ = Command::new("docker")
            .arg("--host")
            .arg(endpoint().as_str())
            .env_remove("DOCKER_CONTEXT")
            .env_remove("DOCKER_HOST")
            .env_remove("DOCKER_TLS")
            .env_remove("DOCKER_TLS_VERIFY")
            .env_remove("DOCKER_CERT_PATH")
            .args(["container", "rm", "--force", &self.0])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = PathBuf::from(
            std::env::var_os("TMPDIR").expect("TMPDIR must name the test scratch directory"),
        )
        .join(unique());
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore = "requires a local Docker engine and alpine:3.22"]
fn capacity_and_full_lifecycle_with_limits_mounts_and_environment() {
    let docker = engine();
    let capacity = docker.capacity().unwrap();
    assert!(capacity.cpu_millicores >= 1000);
    assert!(capacity.memory_bytes > 0);

    let scratch = Scratch::new();
    fs::write(scratch.0.join("input"), "mounted value").unwrap();
    let mut request = spec();
    request
        .environment
        .insert("REGISTRATION_TOKEN", "dummy-secret='quoted' = value")
        .unwrap();
    request.environment.insert("EMPTY", "").unwrap();
    // Exercise concurrent pipe writing, beyond the pipe buffer's capacity.
    for i in 0..32 {
        request
            .environment
            .insert(&format!("BULK_{i}"), &"x".repeat(4096))
            .unwrap();
    }
    request.mounts.push(BindMount {
        source: scratch.0.to_str().unwrap().into(),
        target: "/input".into(),
        read_only: true,
    });
    let sandbox = docker.start(&request).unwrap();
    let _cleanup = Container(sandbox.container_id.clone());
    assert_eq!(sandbox.state, "running");
    assert_eq!(sandbox.sandbox_id, request.sandbox_id);
    assert!(docker.list().unwrap().contains(&sandbox));

    let details: Value =
        serde_json::from_slice(&cli(&["container", "inspect", &sandbox.container_id])).unwrap();
    let details = &details[0];
    assert_eq!(details["HostConfig"]["NanoCpus"], 500_000_000_u64);
    assert_eq!(details["HostConfig"]["Memory"], request.memory_bytes);
    assert_eq!(details["Config"]["Labels"]["homepool.managed"], "true");
    assert_eq!(details["Config"]["Labels"]["homepool.member"], "4");
    assert!(
        !details["Config"]["Labels"]
            .to_string()
            .contains("dummy-secret")
    );
    // Verify values inside the sandbox without passing their contents as args.
    let result = cli(&[
        "container",
        "exec",
        &sandbox.container_id,
        "sh",
        "-c",
        "printf '%s\\n' \"$REGISTRATION_TOKEN\"; cat /input/input; test -z \"$EMPTY\"; test ${#BULK_31} -eq 4096",
    ]);
    assert_eq!(result, b"dummy-secret='quoted' = value\nmounted value");
    assert_eq!(details["Mounts"][0]["RW"], false);

    docker.stop(&sandbox.container_id).unwrap();
    let stopped = docker
        .list()
        .unwrap()
        .into_iter()
        .find(|item| item.container_id == sandbox.container_id)
        .unwrap();
    assert_eq!(stopped.state, "exited");
    docker.remove(&sandbox.container_id).unwrap();
    assert!(
        !docker
            .list()
            .unwrap()
            .iter()
            .any(|item| item.container_id == sandbox.container_id)
    );
}

#[test]
#[ignore = "requires a local Docker engine and alpine:3.22"]
fn foreign_members_and_unmanaged_containers_are_protected() {
    let sandbox = engine().start(&spec()).unwrap();
    let _cleanup = Container(sandbox.container_id.clone());
    let foreign = Docker::new(endpoint(), MemberId(5));
    assert!(
        !foreign
            .list()
            .unwrap()
            .iter()
            .any(|item| item.container_id == sandbox.container_id)
    );
    assert!(matches!(
        foreign.stop(&sandbox.container_id),
        Err(Error::NotManaged)
    ));
    assert!(matches!(
        foreign.remove(&sandbox.container_id),
        Err(Error::NotManaged)
    ));

    let id = String::from_utf8(cli(&[
        "container",
        "create",
        "--label",
        "homepool.member=4",
        "--label",
        "homepool.sandbox=unmanaged",
        "alpine:3.22",
        "true",
    ]))
    .unwrap()
    .trim()
    .to_owned();
    let _unmanaged = Container(id.clone());
    assert!(
        !engine()
            .list()
            .unwrap()
            .iter()
            .any(|item| item.container_id == id)
    );
    assert!(matches!(engine().stop(&id), Err(Error::NotManaged)));
    assert!(matches!(engine().remove(&id), Err(Error::NotManaged)));
}

#[test]
#[ignore = "requires a local Docker engine and alpine:3.22"]
fn failed_start_rolls_back_the_created_container() {
    let mut request = spec();
    request.command = vec!["/no-such-program".into()];
    let error = engine().start(&request).unwrap_err();
    let Error::StartFailed {
        container_id,
        cleanup_failed,
    } = error
    else {
        panic!("expected a start failure");
    };
    let _cleanup = Container(container_id.clone());
    assert!(!cleanup_failed);
    assert!(
        !engine()
            .list()
            .unwrap()
            .iter()
            .any(|item| item.container_id == container_id)
    );
}
