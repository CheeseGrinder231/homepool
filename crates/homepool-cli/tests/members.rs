use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

use serde_json::{Value, json};

// Keep all runtime fixtures in Cargo's scratch directory, never in the source tree.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "members-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn file(&self, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_homepool"));
        command
            .current_dir(&self.0)
            .env_remove("HOMEPOOL_CONFIG")
            .env_remove("XDG_CONFIG_HOME");
        command
    }

    fn status(&self, source: &str) -> Output {
        let path = self.file("members.toml", source);
        self.command()
            .arg("--config")
            .arg(path)
            .args(["status", "--json"])
            .output()
            .unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn decoded(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert!(output.stdout.ends_with(b"\n"));
    serde_json::from_slice(&output.stdout).unwrap()
}

fn diagnostic(output: &Output, expected: &str) {
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains(expected),
        "expected {expected:?}, got {error}"
    );
}

fn member(id: u64, tier: &str, state: &str) -> String {
    format!("[[members]]\nid = {id}\ntier = '{tier}'\nstate = '{state}'\ncapabilities = {{}}\n")
}

#[test]
fn example_covers_tiers_states_capabilities_and_budget_shape() {
    let scratch = Scratch::new();
    let source = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/members.toml"),
    )
    .unwrap();
    let output = scratch.status(&source);
    assert_eq!(
        decoded(&output),
        json!({
            "version": 1,
            "members": [
                {"id": 1, "tier": "core", "state": "active", "capabilities": {
                    "storage_bytes": 107374182400_u64,
                    "compute": {"cpu_millicores": 2000, "memory_bytes": 4294967296_u64, "sandbox_slots": 2}
                }},
                {"id": 2, "tier": "burst", "state": "unreachable", "capabilities": {
                    "storage_bytes": 53687091200_u64, "compute": null
                }},
                {"id": 3, "tier": "burst", "state": "draining", "capabilities": {
                    "storage_bytes": 0,
                    "compute": {"cpu_millicores": 1000, "memory_bytes": 2147483648_u64, "sandbox_slots": 1}
                }}
            ]
        })
    );
    let human = scratch
        .command()
        .args(["status", "--config", "members.toml"])
        .output()
        .unwrap();
    assert!(human.status.success());
    let text = String::from_utf8(human.stdout).unwrap();
    for expected in [
        "Declared members",
        "1  core  active",
        "2000 millicores",
        "4294967296 memory bytes",
        "2 sandbox slots",
        "107374182400 bytes",
        "compute: none",
        "2  burst  unreachable",
        "3  burst  draining",
    ] {
        assert!(text.contains(expected), "missing {expected:?}: {text}");
    }
}

#[test]
fn empty_pool_and_default_capabilities_have_stable_json() {
    let scratch = Scratch::new();
    let empty = scratch.status("version = 1\nmembers = []\n");
    assert_eq!(empty.stdout, b"{\"version\":1,\"members\":[]}\n");
    decoded(&empty);
    assert_eq!(
        decoded(&scratch.status(&format!("version = 1\n{}", member(0, "core", "active")))),
        json!({
            "version": 1, "members": [{"id": 0, "tier": "core", "state": "active", "capabilities": {"compute": null, "storage_bytes": 0}}]
        })
    );
}

#[test]
fn closed_stdout_pipe_reports_output_failure() {
    let scratch = Scratch::new();
    let path = scratch.file("members.toml", "version = 1\nmembers = []\n");
    let (reader, writer) = std::io::pipe().unwrap();
    // Close before spawning so the outcome does not depend on process scheduling
    // or on how much output the platform's pipe buffer can hold.
    drop(reader);
    let output = scratch
        .command()
        .args(["status", "--json", "--config"])
        .arg(path)
        .stdout(writer)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("homepool: stdout:"));
}

#[cfg(target_os = "linux")]
#[test]
fn closing_stdout_pipe_mid_json_leaves_partial_output_and_reports_failure() {
    use std::{io::Read, process::Stdio};

    let scratch = Scratch::new();
    // Since Linux 4.9 this is also the ceiling on a new pipe's capacity.
    // Exceed it so the child cannot finish writing before the reader closes.
    let pipe_ceiling: usize = fs::read_to_string("/proc/sys/fs/pipe-max-size")
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut source = String::from("version = 1\n");
    for id in 0..=pipe_ceiling / 64 {
        source.push_str(&member(id as u64, "core", "active"));
    }
    let complete = scratch.status(&source);
    assert!(complete.status.success());
    let mut prefix = [0; 32];
    assert!(complete.stdout.len() > pipe_ceiling + prefix.len());

    let mut child = scratch
        .command()
        .args(["status", "--json", "--config", "members.toml"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = child.stdout.take().unwrap();
    reader.read_exact(&mut prefix).unwrap();
    drop(reader);
    let output = child.wait_with_output().unwrap();

    assert_eq!(prefix, complete.stdout[..prefix.len()]);
    assert!(serde_json::from_slice::<Value>(&prefix).is_err());
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("homepool: stdout:"));
}

#[test]
fn json_member_order_is_independent_of_input_order() {
    let scratch = Scratch::new();
    let first = member(1, "core", "active");
    let second = member(2, "burst", "draining");
    let a = scratch.status(&format!("version = 1\n{second}{first}"));
    let b = scratch.status(&format!("version = 1\n{first}{second}"));
    assert_eq!(a.stdout, b.stdout);
    assert_eq!(decoded(&a)["members"][0]["id"], 1);
}

#[test]
fn resolution_prefers_explicit_then_environment_then_xdg() {
    let scratch = Scratch::new();
    let explicit = scratch.file(
        "explicit.toml",
        format!("version = 1\n{}", member(1, "core", "active")),
    );
    let configured = scratch.file(
        "configured.toml",
        format!("version = 1\n{}", member(2, "core", "active")),
    );
    scratch.file(
        "xdg/homepool/members.toml",
        format!("version = 1\n{}", member(3, "core", "active")),
    );
    let mut command = scratch.command();
    command
        .args(["status", "--json"])
        .env("HOMEPOOL_CONFIG", configured)
        .env("XDG_CONFIG_HOME", scratch.0.join("xdg"));
    let output = command.arg("--config").arg(explicit).output().unwrap();
    assert_eq!(decoded(&output)["members"][0]["id"], 1);
    let output = scratch
        .command()
        .args(["status", "--json"])
        .env("HOMEPOOL_CONFIG", "configured.toml")
        .env("XDG_CONFIG_HOME", scratch.0.join("xdg"))
        .output()
        .unwrap();
    assert_eq!(decoded(&output)["members"][0]["id"], 2);
    let output = scratch
        .command()
        .args(["status", "--json"])
        .env("XDG_CONFIG_HOME", scratch.0.join("xdg"))
        .output()
        .unwrap();
    assert_eq!(decoded(&output)["members"][0]["id"], 3);
}

#[test]
fn selected_missing_or_invalid_file_never_falls_back() {
    let scratch = Scratch::new();
    let valid = scratch.file("valid.toml", "version = 1\nmembers = []");
    let invalid = scratch.file("invalid.toml", "version = 2\nmembers = []");
    for selected in [scratch.0.join("missing.toml"), invalid] {
        let output = scratch
            .command()
            .args(["status", "--json", "--config"])
            .arg(&selected)
            .env("HOMEPOOL_CONFIG", &valid)
            .output()
            .unwrap();
        diagnostic(&output, &format!("{}:1:", selected.display()));
    }
    scratch.file("xdg/homepool/members.toml", "version = 1\nmembers = []");
    let output = scratch
        .command()
        .args(["status", "--json"])
        .env("HOMEPOOL_CONFIG", "missing.toml")
        .env("XDG_CONFIG_HOME", scratch.0.join("xdg"))
        .output()
        .unwrap();
    diagnostic(&output, "missing.toml:1: <file>:");
    let output = scratch
        .command()
        .args(["status", "--json"])
        .env("XDG_CONFIG_HOME", &scratch.0)
        .output()
        .unwrap();
    diagnostic(&output, "members.toml:1: <file>:");
}

#[test]
fn invalid_or_unset_selection_is_actionable() {
    let scratch = Scratch::new();
    for (variable, value, expected) in [
        ("XDG_CONFIG_HOME", None, "XDG_CONFIG_HOME is unset"),
        (
            "XDG_CONFIG_HOME",
            Some(""),
            "XDG_CONFIG_HOME must be an absolute",
        ),
        (
            "XDG_CONFIG_HOME",
            Some("relative"),
            "XDG_CONFIG_HOME must be an absolute",
        ),
        (
            "HOMEPOOL_CONFIG",
            Some(""),
            "HOMEPOOL_CONFIG: file path must not be empty",
        ),
    ] {
        let mut command = scratch.command();
        command.args(["status", "--json"]);
        if let Some(value) = value {
            command.env(variable, value);
        }
        diagnostic(&command.output().unwrap(), expected);
    }
    scratch.file("valid.toml", "version = 1\nmembers = []");
    let output = scratch
        .command()
        .args(["--config", "valid.toml", "status", "--json"])
        .env("HOMEPOOL_CONFIG", "")
        .env("XDG_CONFIG_HOME", "relative")
        .output()
        .unwrap();
    assert_eq!(decoded(&output), json!({"version": 1, "members": []}));
}

#[test]
fn schema_errors_name_file_line_and_field() {
    let scratch = Scratch::new();
    for (source, location) in [
        ("version = 2\nmembers = []", ":1: version:"),
        ("version = '1'\nmembers = []", ":1: version:"),
        ("members = []", ":1: version: missing required field"),
        ("version = 1", ":1: members: missing required field"),
        ("version = 1\nmembers = {}", ":2: members:"),
        ("version = 1\nmembers = [false]", ":2: members[0]:"),
        (
            "version = 1\nmembers = []\nextra = true",
            ":3: extra: unknown field",
        ),
        (
            "version = 1\n[[members]]\nid = 1\ntier = 'other'\nstate = 'active'\ncapabilities = {}",
            ":4: members[0].tier:",
        ),
        (
            "version = 1\n[[members]]\nid = 1\ntier = 'core'\nstate = 'other'\ncapabilities = {}",
            ":5: members[0].state:",
        ),
        ("version = 1\n[[members]]\nid = -1", ":3: members[0].id:"),
        (
            "version = 1\n[[members]]\nid = 1\ntier = 'core'\nstate = 'active'",
            ":2: members[0].capabilities: missing required field",
        ),
        (
            "version = 1\n[[members]]\nid = 1\ntier = 'core'\nstate = 'active'\ncapabilities = false",
            ":6: members[0].capabilities:",
        ),
        (
            "version = 1\n[[members]]\nid = 1\ntier = 'core'\nstate = 'active'\nextra = 1\ncapabilities = {}",
            ":6: members[0].extra: unknown field",
        ),
    ] {
        diagnostic(&scratch.status(source), &format!("members.toml{location}"));
    }
}

#[test]
fn nested_errors_retain_spans_with_comments_and_quoted_dotted_keys() {
    let scratch = Scratch::new();
    let base =
        "# placeholder: α\nversion = 1\n[[members]]\nid = 1\ntier = 'core'\nstate = 'active'\n";
    for (tail, location) in [
        (
            "capabilities.storage_bytes = -1",
            ":7: members[0].capabilities.storage_bytes:",
        ),
        (
            "capabilities.\"unexpected\" = 1",
            ":7: members[0].capabilities.unexpected: unknown field",
        ),
        (
            "capabilities.compute = false",
            ":7: members[0].capabilities.compute:",
        ),
        (
            "[members.capabilities.compute]\ncpu_millicores = 1\nmemory_bytes = 1",
            ":7: members[0].capabilities.compute.sandbox_slots: missing required field",
        ),
        (
            "[members.capabilities.compute]\ncpu_millicores = 1\nmemory_bytes = 1\nsandbox_slots = 4294967296",
            ":10: members[0].capabilities.compute.sandbox_slots:",
        ),
        (
            "[members.capabilities.compute]\ncpu_millicores = 1\nmemory_bytes = '1 GiB'\nsandbox_slots = 1",
            ":9: members[0].capabilities.compute.memory_bytes:",
        ),
        (
            "[members.capabilities.compute]\ncpu_millicores = 1.5\nmemory_bytes = 1\nsandbox_slots = 1",
            ":8: members[0].capabilities.compute.cpu_millicores:",
        ),
        (
            "[members.capabilities.compute]\nextra = 1",
            ":8: members[0].capabilities.compute.extra: unknown field",
        ),
    ] {
        diagnostic(
            &scratch.status(&format!("{base}{tail}")),
            &format!("members.toml{location}"),
        );
    }
}

#[test]
fn duplicate_id_points_to_the_later_id() {
    let scratch = Scratch::new();
    let source = format!(
        "version = 1\n{}{}",
        member(1, "core", "active"),
        member(1, "burst", "draining")
    );
    diagnostic(
        &scratch.status(&source),
        "members.toml:8: members[1].id: duplicate member id",
    );
}

#[test]
fn unsigned_integer_limits_and_toml_notation_are_preserved() {
    let scratch = Scratch::new();
    let source = "version = 1\n[[members]]\nid = 18446744073709551615\ntier = 'core'\nstate = 'active'\n[members.capabilities]\nstorage_bytes = 18446744073709551615\n[members.capabilities.compute]\ncpu_millicores = +1_000\nmemory_bytes = 0xFFFF\nsandbox_slots = 4294967295";
    let value = decoded(&scratch.status(source));
    assert_eq!(value["members"][0]["id"], u64::MAX);
    assert_eq!(
        value["members"][0]["capabilities"],
        json!({"storage_bytes": u64::MAX, "compute": {"cpu_millicores": 1000, "memory_bytes": 65535, "sandbox_slots": u32::MAX}})
    );
    diagnostic(
        &scratch.status(&source.replace("+1_000", "18446744073709551616")),
        "members.toml:9: members[0].capabilities.compute.cpu_millicores:",
    );
    let zero = source
        .replace("18446744073709551615", "0")
        .replace("+1_000", "0")
        .replace("0xFFFF", "0")
        .replace("4294967295", "0");
    assert_eq!(
        decoded(&scratch.status(&zero))["members"][0]["capabilities"]["compute"],
        json!({"cpu_millicores": 0, "memory_bytes": 0, "sandbox_slots": 0})
    );
}

#[test]
fn invalid_toml_and_non_utf8_files_fail_without_json() {
    let scratch = Scratch::new();
    diagnostic(
        &scratch.status("version = 1\nmembers = [\n!"),
        "members.toml:3: <document>:",
    );
    let path = scratch.file("non-utf8.toml", [0xff]);
    let output = scratch
        .command()
        .args(["status", "--json", "--config"])
        .arg(path)
        .output()
        .unwrap();
    diagnostic(&output, "non-utf8.toml:1: <file>:");
}

#[test]
fn bad_arguments_fail_and_help_needs_no_file() {
    let scratch = Scratch::new();
    for args in [
        vec![],
        vec!["status", "--config"],
        vec!["status", "--config", ""],
        vec!["status", "--json", "--json"],
        vec!["status", "--config", "a", "--config", "b"],
        vec!["other"],
    ] {
        let output = scratch.command().args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
    let output = scratch.command().arg("--help").output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage: homepool"));
}
