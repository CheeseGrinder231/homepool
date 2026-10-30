//! Opt-in real Garage integration through the shipped CLI.
#[path = "../../adapters/homepool-adapter-garage/tests/support/mod.rs"]
mod support;
use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    process::{Command, Output},
    thread,
    time::{Duration, Instant},
};
use support::Installation;

fn command(installation: &Installation) -> Command {
    let mut paths = vec![installation.scratch.clone()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let mut command = Command::new(env!("CARGO_BIN_EXE_homepool"));
    command
        .current_dir(&installation.scratch)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("GARAGE_TEST_CONTAINER", &installation.container)
        .env_remove("HOMEPOOL_CONFIG")
        .env_remove("HOMEPOOL_SOCKET")
        .args([
            "--config",
            "members.toml",
            "--garage-config",
            "/etc/garage.toml",
        ]);
    command
}
fn success(output: Output) {
    assert!(output.status.success(), "storage command must succeed");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}
#[test]
#[ignore = "requires Docker; CI runs explicitly with TMPDIR set"]
fn shipped_storage_commands_and_s3_credentials_file() {
    let installation = Installation::new();
    let mut members = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(installation.scratch.join("members.toml"))
        .unwrap();
    members.write_all(b"version = 1\n[[members]]\nid = 1\ntier = 'core'\nstate = 'active'\n[members.capabilities]\nstorage_bytes = 1073741824\n").unwrap();
    let wrapper = installation.scratch.join("garage");
    fs::write(
        &wrapper,
        "#!/bin/sh\nexec docker exec -i \"$GARAGE_TEST_CONTAINER\" /garage \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = installation.raw("GetClusterStatus", Value::Null) {
            break status;
        }
        assert!(Instant::now() < deadline, "Garage did not become ready");
        thread::sleep(Duration::from_millis(200));
    };
    let node = status["nodes"][0]["id"].as_str().unwrap();
    success(
        command(&installation)
            .args(["stage-storage", "1", node, "zone-a"])
            .output()
            .unwrap(),
    );
    let before = installation.raw("GetClusterLayout", Value::Null).unwrap();
    let unknown = if node == "0".repeat(64) {
        "f".repeat(64)
    } else {
        "0".repeat(64)
    };
    let output = command(&installation)
        .args(["stage-storage", "1", &unknown, "zone-a"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        output.stderr,
        b"homepool: Garage storage node is not known to the cluster\n"
    );
    assert_eq!(
        installation.raw("GetClusterLayout", Value::Null).unwrap(),
        before
    );
    let output = command(&installation)
        .args(["apply-storage", "99"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        output.stderr,
        b"homepool: Garage operation failed; inspect the installation privately\n"
    );
    success(
        command(&installation)
            .args(["apply-storage", "1"])
            .output()
            .unwrap(),
    );
    let layout = installation.raw("GetClusterLayout", Value::Null).unwrap();
    assert_eq!(layout["roles"][0]["capacity"], 1_073_741_824_u64);
    success(
        command(&installation)
            .args([
                "create-consumer",
                "1",
                "--credentials-file",
                "credentials.json",
            ])
            .output()
            .unwrap(),
    );
    let path = installation.scratch.join("credentials.json");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    let credentials: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    for action in ["put", "get"] {
        assert!(
            installation.s3(
                credentials["aws_access_key_id"].as_str().unwrap(),
                credentials["aws_secret_access_key"].as_str().unwrap(),
                credentials["bucket"].as_str().unwrap(),
                action,
            ),
            "fresh S3 client must succeed with credentials from the file"
        );
    }
    let keys = installation.raw("ListKeys", Value::Null).unwrap();
    let output = command(&installation)
        .args([
            "create-consumer",
            "2",
            "--credentials-file",
            "credentials.json",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"homepool: could not create new private credentials file; destination must not exist\n"
    );
    assert!(installation.raw("ListKeys", Value::Null).unwrap() == keys);
}
