//! Storage commands run the shipped binary; fake Garage never receives secrets in argv.
use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, symlink},
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

const MOCK: &str = r#"#!/bin/sh
: >garage-called
cat >/dev/null
[ "$FAIL_GARAGE" != 1 ] || exit 1
printf 'untrusted-private-diagnostic' >&2
case "$4" in
GetClusterStatus) printf '{"nodes":[{"id":"%s"}]}' "$KNOWN_NODE" ;;
UpdateClusterLayout) printf '{}' ;;
ApplyClusterLayout) exit 1 ;;
CreateBucket)
  if [ "$CHECK_CREDENTIALS" = 1 ]; then
    [ "$(stat -c %a credentials.json)" = 600 ] || exit 1
  fi
  printf '{"id":"bucket-id"}' ;;
CreateKey) printf '{"accessKeyId":"placeholder-access-key","secretAccessKey":"placeholder-secret-key"}' ;;
AllowBucketKey) printf '{}' ;;
*) exit 1 ;;
esac
"#;

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = PathBuf::from(
            std::env::var_os("TMPDIR").unwrap_or_else(|| env!("CARGO_TARGET_TMPDIR").into()),
        )
        .join(format!(
            "storage-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)
            .unwrap();
        let scratch = Self(root);
        scratch.write("members.toml", &members(1_073_741_824));
        scratch.write("garage", MOCK);
        fs::set_permissions(scratch.0.join("garage"), fs::Permissions::from_mode(0o700)).unwrap();
        scratch
    }
    fn write(&self, name: &str, contents: &str) {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(self.0.join(name))
            .unwrap();
        file.write_all(contents.as_bytes()).unwrap();
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_homepool"));
        self.configure(&mut command);
        command
    }
    fn configure(&self, command: &mut Command) {
        let mut paths = vec![self.0.clone()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .current_dir(&self.0)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("HOMEPOOL_CONFIG", "members.toml")
            .env("KNOWN_NODE", "a".repeat(64))
            .env_remove("HOMEPOOL_SOCKET")
            .env_remove("XDG_RUNTIME_DIR")
            .args(["--garage-config", "private-garage.toml"]);
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn not_called(&self) {
        assert!(
            !self.0.join("garage-called").exists(),
            "Garage must not be called"
        );
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn members(storage: u64) -> String {
    format!(
        "version = 1\n[[members]]\nid = 1\ntier = 'core'\nstate = 'active'\n[members.capabilities]\nstorage_bytes = {storage}\n"
    )
}
fn success(output: &Output) {
    assert!(output.status.success(), "command must succeed");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}
fn failure(output: &Output, message: &str) {
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        format!("homepool: {message}\n")
    );
}
#[test]
fn consumer_credentials_are_private_complete_and_only_in_the_file() {
    let scratch = Scratch::new();
    success(
        &scratch
            .command()
            .args([
                "create-consumer",
                "7",
                "--credentials-file",
                "credentials.json",
            ])
            .env("CHECK_CREDENTIALS", "1")
            .output()
            .unwrap(),
    );
    let path = scratch.0.join("credentials.json");
    let metadata = fs::symlink_metadata(&path).unwrap();
    assert!(metadata.is_file());
    assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
    let credentials: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    // Boolean checks avoid printing credential bytes on a test assertion failure.
    assert!(credentials["version"] == 1);
    assert!(credentials["bucket"] == "homepool-consumer-7");
    assert!(credentials["aws_access_key_id"] == "placeholder-access-key");
    assert!(credentials["aws_secret_access_key"] == "placeholder-secret-key");
}
#[test]
fn existing_entries_and_insecure_parents_fail_before_garage() {
    for kind in ["file", "directory", "symlink", "insecure-parent"] {
        let scratch = Scratch::new();
        match kind {
            "file" => scratch.write("credentials.json", "keep-existing-content"),
            "directory" => fs::create_dir(scratch.0.join("credentials.json")).unwrap(),
            "symlink" => symlink("missing", scratch.0.join("credentials.json")).unwrap(),
            _ => fs::set_permissions(&scratch.0, fs::Permissions::from_mode(0o755)).unwrap(),
        }
        let output = scratch.run(&[
            "create-consumer",
            "7",
            "--credentials-file",
            "credentials.json",
        ]);
        failure(
            &output,
            if kind == "insecure-parent" {
                "private path must be owned by the current user with mode 0700 (directory) or 0600 (file)"
            } else {
                "could not create new private credentials file; destination must not exist"
            },
        );
        scratch.not_called();
        if kind == "file" {
            assert_eq!(
                fs::read_to_string(scratch.0.join("credentials.json")).unwrap(),
                "keep-existing-content"
            );
        }
    }
}
#[test]
fn unknown_and_zero_storage_members_do_not_call_garage() {
    let scratch = Scratch::new();
    failure(
        &scratch.run(&["stage-storage", "2", &"a".repeat(64), "zone-a"]),
        "unknown storage member",
    );
    scratch.not_called();
    scratch.write("members.toml", &members(0));
    failure(
        &scratch.run(&["stage-storage", "1", &"a".repeat(64), "zone-a"]),
        "Garage portion requires storage, a full node ID and a zone",
    );
    scratch.not_called();
}
#[test]
fn stages_and_sanitizes_unknown_nodes_and_wrong_versions() {
    let scratch = Scratch::new();
    success(&scratch.run(&["stage-storage", "1", &"a".repeat(64), "zone-a"]));
    failure(
        &scratch.run(&["stage-storage", "1", &"b".repeat(64), "zone-a"]),
        "Garage storage node is not known to the cluster",
    );
    failure(
        &scratch.run(&["apply-storage", "99"]),
        "Garage operation failed; inspect the installation privately",
    );
}
#[test]
fn credentials_write_failure_warns_that_consumer_may_exist() {
    let scratch = Scratch::new();
    // Ignore SIGXFSZ across exec so the zero file-size limit yields a write error.
    let mut command = Command::new("sh");
    command.args([
        "-c",
        "ulimit -f 0; trap '' XFSZ; exec \"$@\"",
        "sh",
        env!("CARGO_BIN_EXE_homepool"),
    ]);
    scratch.configure(&mut command);
    let output = command
        .args([
            "create-consumer",
            "7",
            "--credentials-file",
            "credentials.json",
        ])
        .output()
        .unwrap();
    failure(
        &output,
        "Garage issued credentials but the credentials file could not be written; the consumer may exist; inspect Garage privately before retrying",
    );
    assert!(scratch.0.join("garage-called").exists());
    assert!(!scratch.0.join("credentials.json").exists());
}
#[test]
fn member_and_argument_failures_do_not_echo_private_input() {
    let scratch = Scratch::new();
    scratch.write("members.toml", "invalid private document");
    failure(
        &scratch.run(&["apply-storage", "1"]),
        "invalid private member file",
    );
    scratch.not_called();
    failure(
        &scratch.run(&[
            "create-consumer",
            "1",
            "--credentials-file",
            "credentials.json",
            "--private-unknown-option",
        ]),
        "unexpected argument",
    );
    scratch.not_called();
}

#[test]
fn failed_garage_provisioning_removes_reserved_file() {
    let scratch = Scratch::new();
    let output = scratch
        .command()
        .env("FAIL_GARAGE", "1")
        .args([
            "create-consumer",
            "7",
            "--credentials-file",
            "credentials.json",
        ])
        .output()
        .unwrap();
    failure(
        &output,
        "Garage operation failed; inspect the installation privately",
    );
    assert!(scratch.0.join("garage-called").exists());
    assert!(!scratch.0.join("credentials.json").exists());
}

#[test]
fn storage_argument_requirements_fail_before_garage() {
    let scratch = Scratch::new();
    for (args, message) in [
        (
            vec!["apply-storage"],
            "storage command requires its documented operands",
        ),
        (
            vec!["apply-storage", "private-invalid-version"],
            "storage identity or version must be an unsigned integer",
        ),
        (
            vec!["stage-storage", "1", "short-node", "zone-a"],
            "Garage portion requires storage, a full node ID and a zone",
        ),
        (
            vec!["create-consumer", "1"],
            "create-consumer requires --credentials-file FILE",
        ),
        (
            vec!["apply-storage", "1", "--socket", "private-socket"],
            "--socket is not supported by storage commands",
        ),
        (
            vec!["apply-storage", "1", "--credentials-file", "private-file"],
            "--credentials-file is only supported by create-consumer",
        ),
    ] {
        failure(&scratch.run(&args), message);
        scratch.not_called();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_homepool"))
        .args(["apply-storage", "1"])
        .output()
        .unwrap();
    failure(&output, "storage commands require --garage-config FILE");
}

// Linux CI uses dynamic libc fsync interposition to inject faults at the exact
// persistence boundary, without production hooks or sleeps racing the signal.
#[cfg(target_os = "linux")]
impl Scratch {
    fn credentials_sync_fault(&self, fault: &str) -> Command {
        let library = self.0.join("credentials-sync.so");
        let output = Command::new("cc")
            .args(["-shared", "-fPIC", "-Wall", "-Werror", "-o"])
            .arg(&library)
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/support/credentials_sync.c"
            ))
            .output()
            .expect("C compiler is required for deterministic sync fault tests");
        assert!(
            output.status.success(),
            "could not compile sync fault fixture"
        );
        let mut command = self.command();
        command
            .env("LD_PRELOAD", library)
            .env("HOMEPOOL_TEST_SYNC_FAULT", fault)
            .args([
                "create-consumer",
                "7",
                "--credentials-file",
                "credentials.json",
            ]);
        command
    }
}

#[cfg(target_os = "linux")]
#[test]
fn sigkill_leaves_unconfirmed_credentials_that_refuse_retry() {
    use std::os::unix::process::ExitStatusExt;

    let scratch = Scratch::new();
    let output = scratch.credentials_sync_fault("kill").output().unwrap();
    assert_eq!(output.status.signal(), Some(signal_hook::consts::SIGKILL));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(scratch.0.join("credentials-sync-reached").exists());
    let path = scratch.0.join("credentials.json");
    let unconfirmed = fs::read(&path).unwrap();
    assert!(serde_json::from_slice::<Value>(&unconfirmed).is_ok());
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    fs::remove_file(scratch.0.join("garage-called")).unwrap();
    // Earlier termination windows can leave an empty file or partial JSON.
    // Every surviving state must block retry without altering it or calling Garage.
    for leftover in [unconfirmed, Vec::new(), b"{\"version\":".to_vec()] {
        fs::write(&path, &leftover).unwrap();
        failure(
            &scratch.run(&[
                "create-consumer",
                "7",
                "--credentials-file",
                "credentials.json",
            ]),
            "could not create new private credentials file; destination must not exist",
        );
        scratch.not_called();
        assert!(
            fs::read(&path).unwrap() == leftover,
            "retry must preserve surviving credentials"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn sigterm_during_credentials_sync_error_preserves_recovery_warning() {
    let scratch = Scratch::new();
    let output = scratch
        .credentials_sync_fault("term-error")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(143));
    assert!(output.stdout.is_empty());
    assert!(scratch.0.join("credentials-sync-reached").exists());
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(!diagnostic.contains("placeholder-access-key"));
    assert!(!diagnostic.contains("placeholder-secret-key"));
    // Do not echo unexpected diagnostics: a regression could contain credentials.
    assert!(
        diagnostic
            == "homepool: Garage issued credentials but the credentials file could not be written; the consumer may exist; inspect Garage privately before retrying\n",
        "must preserve the specific consumer recovery warning"
    );
    assert!(!scratch.0.join("credentials.json").exists());
}
