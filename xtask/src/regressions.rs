use super::*;
use std::{
    fs,
    io::Write,
    net::{Ipv4Addr, Ipv6Addr},
    path::PathBuf,
    process::{Command, Stdio},
};

struct Repository(history::Snapshot);

impl Repository {
    fn new() -> Self {
        let repo = Self(history::Snapshot::empty().unwrap());
        repo.git(&["init", "--initial-branch=main"]);
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        repo.git(&[
            "remote",
            "add",
            "origin",
            &history::repository_url(source).unwrap(),
        ]);
        repo.write(".gitignore", "target/\n");
        repo.write("Cargo.toml", "[workspace]\nmembers = []\nresolver = '3'\n\n[package]\nname = 'fixture-root'\nversion = '0.1.0'\nedition = '2024'\n");
        repo.write("src/lib.rs", "pub fn value() -> u8 { 1 }\n");
        repo.write("structure/layers.toml", "[layers]\ncore = []\nadapter = ['core']\napp = ['core', 'adapter']\n[crates]\nfixture-root = 'core'\n");
        repo.write(
            "structure/baseline.toml",
            "layer_violations = 0\ninstance_literals = 0\nallowance_count = 0\n",
        );
        repo.write("structure/allowances.toml", "allowances = []\n");
        let output = Command::new("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(repo.root())
            .output()
            .unwrap();
        assert!(output.status.success());
        repo.commit();
        repo
    }

    fn root(&self) -> &Path {
        self.0.root()
    }
    fn write(&self, path: &str, text: &str) {
        let path = self.root().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn git(&self, args: &[&str]) -> String {
        String::from_utf8(history::git(self.root(), args).unwrap())
            .unwrap()
            .trim()
            .into()
    }
    fn commit(&self) -> String {
        self.git(&["add", "."]);
        let tree = self.git(&["write-tree"]);
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let original =
            String::from_utf8(history::git(source, &["cat-file", "commit", "HEAD"]).unwrap())
                .unwrap();
        let identities: Vec<_> = original
            .lines()
            .filter(|line| line.starts_with("author ") || line.starts_with("committer "))
            .collect();
        assert_eq!(identities.len(), 2);
        let mut commit = format!("tree {tree}\n");
        if let Ok(parent) = history::git(self.root(), &["rev-parse", "--verify", "HEAD"]) {
            commit.push_str(&format!(
                "parent {}\n",
                String::from_utf8(parent).unwrap().trim()
            ));
        }
        // Fixture objects retain the repository commit's identity verbatim;
        // tests neither require nor override the runner's Git identity.
        commit.push_str(&format!(
            "{}\n{}\n\nRecord fixture state\n",
            identities[0], identities[1]
        ));
        let mut child = Command::new("git")
            .args(["hash-object", "-t", "commit", "-w", "--stdin"])
            .current_dir(self.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(commit.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let head = String::from_utf8(output.stdout).unwrap().trim().to_owned();
        self.git(&["update-ref", "HEAD", &head]);
        self.git(&["update-ref", "refs/remotes/origin/main", &head]);
        head
    }
    fn allowances(&self, reason: &str) {
        let repository = history::repository_url(self.root()).unwrap();
        self.write("structure/allowances.toml", &format!("[[allowances]]\nid = 'literal'\nmetric = 'instance_literals'\namount = 2\nreason = '{reason}'\nissue = '{repository}/issues/1'\n"));
    }
}

#[test]
fn root_package_and_relocated_modules_fail_the_end_to_end_check() {
    let repo = Repository::new();
    assert!(check(repo.root(), Some("HEAD")).is_ok());
    let address = Ipv4Addr::new(10, 20, 30, 40);
    for (file, source) in [
        (
            "src/lib.rs",
            format!("pub const ADDRESS: &str = \"{address}\";\n"),
        ),
        (
            "support/relocated.rs",
            format!("pub const ADDRESS: &str = \"{address}\";\n"),
        ),
        ("tests/integration.rs", format!("// {address}\n")),
        ("build.rs", format!("fn main() {{}} // {address}\n")),
    ] {
        repo.write(file, &source);
        if file == "support/relocated.rs" {
            repo.write(
                "src/lib.rs",
                "#[path = \"../support/relocated.rs\"]\npub mod relocated;\n",
            );
        }
        repo.git(&["add", file]);
        let error = check(repo.root(), Some("HEAD")).unwrap_err();
        assert!(error.contains("instance_literals"), "{file}: {error}");
        if file == "src/lib.rs" {
            repo.write(file, "pub fn value() -> u8 { 1 }\n");
        } else {
            fs::remove_file(repo.root().join(file)).unwrap();
        }
    }
}

#[test]
fn explicit_text_and_binary_exclusions_do_not_hide_other_sources() {
    let repo = Repository::new();
    let source = format!("// {}\n", Ipv4Addr::new(10, 20, 30, 40));
    for file in [
        "xtask/tests/fixtures/input.rs",
        "docs/example.txt",
        "NOTES.md",
        "LICENSE",
    ] {
        repo.write(file, &source);
        repo.git(&["add", "--force", file]);
    }
    repo.write("binary.dat", &format!("\0{source}"));
    repo.git(&["add", "binary.dat"]);
    let files = history::working_files(repo.root()).unwrap();
    assert!(literals::scan(repo.root(), &files, "").unwrap().is_empty());
    repo.write("untracked.rs", &source);
    assert_eq!(
        literals::scan(
            repo.root(),
            &history::working_files(repo.root()).unwrap(),
            ""
        )
        .unwrap()
        .len(),
        1
    );
}

#[test]
fn exact_tree_ignores_export_attributes_and_preserves_allowance_history() {
    let repo = Repository::new();
    repo.write(
        "src/lib.rs",
        &format!(
            "pub const ADDRESS: &str = \"{}\";\n",
            Ipv4Addr::new(10, 20, 30, 40)
        ),
    );
    repo.allowances("original");
    repo.write(".gitattributes", "Cargo.toml export-ignore\nstructure/allowances.toml export-ignore\nmarker.txt export-subst\n");
    repo.write("marker.txt", "$Format:%H$\n");
    let base = repo.commit();
    let snapshot = history::Snapshot::create(repo.root(), &base).unwrap();
    assert!(snapshot.files.contains(&PathBuf::from("Cargo.toml")));
    assert_eq!(
        fs::read_to_string(snapshot.root().join("marker.txt")).unwrap(),
        "$Format:%H$\n"
    );
    repo.allowances("edited");
    assert!(
        check(repo.root(), Some(&base))
            .unwrap_err()
            .contains("immutable")
    );
}

#[test]
fn clean_main_checks_its_committed_change_against_the_parent() {
    let repo = Repository::new();
    let parent = repo.git(&["rev-parse", "HEAD"]);
    repo.write("note.txt", "fixture\n");
    repo.commit();
    assert_eq!(
        history::base(repo.root(), None).unwrap(),
        history::Base::Commit(parent)
    );
}

#[test]
fn dirty_main_compares_allowance_edits_against_head() {
    let repo = Repository::new();
    repo.write(
        "src/lib.rs",
        &format!(
            "pub const ADDRESS: &str = \"{}\";\n",
            Ipv4Addr::new(10, 20, 30, 40)
        ),
    );
    repo.allowances("original");
    let head = repo.commit();
    repo.allowances("edited");
    assert_eq!(
        history::base(repo.root(), None).unwrap(),
        history::Base::Commit(head.clone())
    );
    assert!(check(repo.root(), None).unwrap_err().contains("immutable"));
    repo.git(&["add", "structure/allowances.toml"]);
    assert_eq!(
        history::base(repo.root(), None).unwrap(),
        history::Base::Commit(head)
    );
    assert!(check(repo.root(), None).unwrap_err().contains("immutable"));
}

#[test]
fn root_commit_and_zero_before_sha_have_an_explicit_initial_state() {
    let repo = Repository::new();
    assert_eq!(
        history::base(repo.root(), None).unwrap(),
        history::Base::Initial
    );
    assert!(check(repo.root(), None).is_ok());
    repo.git(&["update-ref", "-d", "refs/remotes/origin/main"]);
    assert_eq!(
        history::base(repo.root(), None).unwrap(),
        history::Base::Initial
    );
    assert!(check(repo.root(), None).is_ok());
    assert!(history::base(repo.root(), Some("missing-revision")).is_err());
    for length in [40, 64] {
        let zero = "0".repeat(length);
        assert_eq!(
            history::base(repo.root(), Some(&zero)).unwrap(),
            history::Base::Initial
        );
        assert!(check(repo.root(), Some(&zero)).is_ok());
    }
}

#[test]
fn compilable_hexadecimal_rust_paths_are_not_ipv6_but_prose_still_is() {
    let repo = Repository::new();
    let path = ["abc", "def"].join(&":".repeat(2));
    let code = format!("mod abc {{ pub fn def() {{}} }}\npub fn call() {{ {path}(); }}\n");
    repo.write("src/lib.rs", &code);
    let output = Command::new("cargo")
        .args(["check", "--offline", "--locked"])
        .current_dir(repo.root())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(check(repo.root(), Some("HEAD")).is_ok());
    for text in [
        format!("// {path}\n"),
        format!("/* outer /* nested */ {path} */\n"),
        format!("pub const A: &str = \"{path}\";\n"),
        format!("pub const A: &str = r###\"{path}\"###;\n"),
        format!("pub const A: &[u8] = br#\"{path}\"#;\n"),
        format!("pub const A: &std::ffi::CStr = c\"{path}\";\n"),
        format!("pub const A: &std::ffi::CStr = cr#\"{path}\"#;\n"),
    ] {
        repo.write("src/lib.rs", &text);
        assert!(
            check(repo.root(), Some("HEAD"))
                .unwrap_err()
                .contains("instance_literals")
        );
    }
}

#[test]
fn scanner_covers_punctuation_case_and_supported_home_paths() {
    let address = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1);
    let root_home = format!("/{}", "root");
    for input in [
        format!("// endpoint {address}."),
        format!(
            "// endpoint {}.",
            Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 0)
        ),
        format!("// endpoint {}.", Ipv6Addr::UNSPECIFIED),
        format!("{}://192.0.2.1", "HTTP"),
        format!("{}://192.0.2.1", "HtTpS"),
        root_home.clone(),
        format!("{root_home}/data"),
        format!("C:\\{}\\example\\data", "Users"),
        format!("D:/{}/example", "Users"),
        format!("C:\\\\{}\\\\example", "Users"),
    ] {
        assert_eq!(literals::in_line(&input).len(), 1, "{input}");
    }
    assert!(literals::in_line(&format!("{root_home}ed")).is_empty());
}

#[test]
fn only_exact_package_repository_metadata_is_exempt() {
    let repo = Repository::new();
    let canonical = history::repository_url(repo.root()).unwrap();
    let manifest = fs::read_to_string(repo.root().join("Cargo.toml")).unwrap();
    for field in [
        format!("repository = '{canonical}'"),
        format!("[workspace.package]\nrepository = '{canonical}'"),
    ] {
        repo.write("Cargo.toml", &format!("{manifest}\n{field}\n"));
        assert!(check(repo.root(), Some("HEAD")).is_ok());
    }
    for field in [
        format!("homepage = '{canonical}'"),
        format!("repository = '{canonical}/extra'"),
        format!("repository = '{canonical}' # {canonical}"),
        format!("# {canonical}"),
    ] {
        repo.write("Cargo.toml", &format!("{manifest}\n{field}\n"));
        assert!(
            check(repo.root(), Some("HEAD"))
                .unwrap_err()
                .contains("instance_literals")
        );
    }
    repo.write("Cargo.toml", &manifest);
    repo.write(
        "src/lib.rs",
        &format!("pub const URL: &str = \"{canonical}\";\n"),
    );
    assert!(
        check(repo.root(), Some("HEAD"))
            .unwrap_err()
            .contains("instance_literals")
    );
}

#[test]
fn dependency_action_is_pinned_to_the_release_commit_in_both_workflows() {
    for workflow in [
        include_str!("../../.github/workflows/ci.yml"),
        include_str!("../../.github/workflows/advisories.yml"),
    ] {
        let pins: Vec<_> = workflow
            .lines()
            .filter_map(|line| {
                line.split_once("uses: EmbarkStudios/cargo-deny-action@")
                    .map(|(_, pin)| pin)
            })
            .collect();
        assert_eq!(pins, ["3c6349835b2b7b196a839186cb8b78e02f7b5f25 # v2.1.1"]);
    }
}

#[test]
fn repository_exception_does_not_cover_unrelated_manifest_tables() {
    let repo = Repository::new();
    let canonical = history::repository_url(repo.root()).unwrap();
    for table in [
        "package.metadata",
        "dependencies.example",
        "workspace.metadata",
    ] {
        repo.write(
            "Cargo.toml",
            &format!("[{table}]\nrepository = '{canonical}'\n"),
        );
        let findings =
            literals::scan(repo.root(), &[PathBuf::from("Cargo.toml")], &canonical).unwrap();
        assert_eq!(findings.len(), 1, "{table}");
    }
}

#[test]
fn a_single_issue_linked_allowance_covers_debt_and_its_own_entry_end_to_end() {
    let repo = Repository::new();
    repo.write(
        "src/lib.rs",
        &format!(
            "pub const ADDRESS: &str = \"{}\";\n",
            Ipv4Addr::new(10, 20, 30, 40)
        ),
    );
    repo.allowances("temporary fixture debt");
    assert!(check(repo.root(), Some("HEAD")).is_ok());
    repo.commit();
    repo.write(
        "src/lib.rs",
        &format!(
            "pub const ADDRESSES: [&str; 2] = [\"{}\", \"{}\"];\n",
            Ipv4Addr::new(10, 20, 30, 40),
            Ipv4Addr::new(10, 20, 30, 41)
        ),
    );
    assert!(
        check(repo.root(), Some("HEAD"))
            .unwrap_err()
            .contains("new allowances")
    );
}

#[test]
fn a_shallow_boundary_is_not_a_true_root_commit() {
    let repo = Repository::new();
    let parent = repo.git(&["rev-parse", "HEAD"]);
    repo.write("note.txt", "fixture\n");
    let head = repo.commit();
    repo.write(".git/shallow", &format!("{head}\n"));
    assert_eq!(
        history::base(repo.root(), None).unwrap(),
        history::Base::Commit(parent)
    );
}

#[test]
fn expanding_macro_literal_suffixes_cannot_hide_an_ipv6_string() {
    let repo = Repository::new();
    let address = Ipv6Addr::new(0xab, 0, 0, 0, 0, 0, 0, 0xdef);
    repo.write("src/lib.rs", &format!(r###"
macro_rules! expose {{
    ($first:literal $second:literal $value:literal) => {{ pub const ADDRESS: &str = $value; }};
}}
expose!("tag"r"\"" "{address}");
#[test]
fn expanded_value() {{
    assert_eq!(ADDRESS.parse::<std::net::Ipv6Addr>().unwrap(), std::net::Ipv6Addr::new(0xab, 0, 0, 0, 0, 0, 0, 0xdef));
}}
"###));
    let output = Command::new("cargo")
        .args(["test", "--offline", "--locked"])
        .current_dir(repo.root())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        check(repo.root(), Some("HEAD"))
            .unwrap_err()
            .contains("instance_literals")
    );
}

#[test]
fn shebang_text_is_scanned_without_confusing_inner_attributes() {
    let repo = Repository::new();
    let address = Ipv6Addr::new(0xab, 0, 0, 0, 0, 0, 0, 0xdef);
    repo.write("src/lib.rs", &format!("#!{address}\npub fn value() {{}}\n"));
    let findings = literals::scan(repo.root(), &[PathBuf::from("src/lib.rs")], "").unwrap();
    assert_eq!(findings, ["src/lib.rs:1:3: IPv6 literal"]);
    repo.write("src/lib.rs", "#![forbid(unsafe_code)]\npub fn value() {}\n");
    assert!(check(repo.root(), Some("HEAD")).is_ok());
}

#[test]
fn compilable_range_paths_are_never_scanned_as_address_text() {
    let repo = Repository::new();
    let path = ["abc", "DEF"].join(&":".repeat(2));
    repo.write("src/lib.rs", &format!("mod abc {{ pub const DEF: u8 = 7; }}\npub fn limit() -> std::ops::RangeTo<u8> {{ ..{path} }}\n"));
    let output = Command::new("cargo")
        .args(["check", "--offline", "--locked"])
        .current_dir(repo.root())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(check(repo.root(), Some("HEAD")).is_ok());
}

#[test]
fn relative_path_components_are_not_absolute_home_paths() {
    let repo = Repository::new();
    let absolute = format!("/{}/data", "root");
    for path in [
        "fixtures/root/data".to_owned(),
        format!("fixtures/{}/example", "home"),
        format!("../{}/example", "Users"),
        format!("fixtures/C:\\{}\\example", "Users"),
    ] {
        repo.write(
            "src/lib.rs",
            &format!("pub const RELATIVE: &str = {path:?};\n"),
        );
        assert!(check(repo.root(), Some("HEAD")).is_ok(), "{path}");
    }
    repo.write(
        "src/lib.rs",
        &format!("pub const ABSOLUTE: &str = {absolute:?};\n"),
    );
    assert!(
        check(repo.root(), Some("HEAD"))
            .unwrap_err()
            .contains("instance_literals")
    );
}

#[cfg(unix)]
#[test]
fn historical_metadata_preserves_an_executable_compiler_wrapper() {
    use std::os::unix::fs::PermissionsExt;
    let repo = Repository::new();
    repo.write(
        ".cargo/config.toml",
        "[build]\nrustc-wrapper = './compiler-wrapper'\n",
    );
    repo.write("compiler-wrapper", "#!/bin/sh\nexec \"$@\"\n");
    fs::set_permissions(
        repo.root().join("compiler-wrapper"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(layers::metadata(repo.root()).is_ok());
    let revision = repo.commit();
    assert!(
        repo.git(&["ls-tree", "HEAD", "compiler-wrapper"])
            .starts_with("100755")
    );
    let snapshot = history::Snapshot::create(repo.root(), &revision).unwrap();
    assert_eq!(
        fs::metadata(snapshot.root().join("compiler-wrapper"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(
        fs::metadata(snapshot.root().join("src/lib.rs"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644
    );
    assert!(layers::metadata(snapshot.root()).is_ok());
}

#[test]
fn bare_tokens_and_tracked_build_inputs_are_scanned_with_compiled_value_controls() {
    let repo = Repository::new();
    let address = Ipv4Addr::new(10, 20, 30, 40).to_string();
    repo.write("inputs/address.txt", &address);
    repo.write(
        "inputs/address.code",
        &format!("pub const INCLUDED: &str = {address:?};\n"),
    );
    repo.write(
        ".cargo/config.toml",
        &format!("[env]\nFIXTURE_ADDRESS = {address:?}\n"),
    );
    repo.write(
        "src/lib.rs",
        &format!(
            r###"
#[doc = stringify!({address})]
pub const DIRECT: &str = stringify!({address});
macro_rules! forward {{ ($($tokens:tt)*) => {{ stringify!($($tokens)*) }}; }}
pub const FORWARDED: &str = forward!({address});
pub const TEXT: &str = include_str!("../inputs/address.txt");
pub const BYTES: &[u8] = include_bytes!("../inputs/address.txt");
include!("../inputs/address.code");
#[doc = include_str!("../inputs/address.txt")]
pub struct Documented;
pub const ENVIRONMENT: &str = env!("FIXTURE_ADDRESS");
#[test]
fn compiled_values() {{
    let expected = std::net::Ipv4Addr::new(10, 20, 30, 40).to_string();
    for value in [DIRECT, FORWARDED, TEXT, INCLUDED, ENVIRONMENT] {{ assert_eq!(value, expected); }}
    assert_eq!(BYTES, expected.as_bytes());
}}
"###
        ),
    );
    repo.git(&["add", "."]);
    let output = Command::new("cargo")
        .args(["test", "--offline", "--locked"])
        .current_dir(repo.root())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let findings = literals::scan(
        repo.root(),
        &history::working_files(repo.root()).unwrap(),
        "",
    )
    .unwrap();
    assert_eq!(findings.len(), 6, "{findings:?}");
    assert_eq!(
        findings
            .iter()
            .filter(|f| f.starts_with("src/lib.rs:"))
            .count(),
        3
    );
    for file in [
        "inputs/address.txt",
        "inputs/address.code",
        ".cargo/config.toml",
    ] {
        assert!(
            findings.iter().any(|f| f.starts_with(&format!("{file}:"))),
            "{file}"
        );
    }
    assert!(
        check(repo.root(), Some("HEAD"))
            .unwrap_err()
            .contains("instance_literals")
    );
}

#[test]
fn only_the_exact_lockfile_registry_source_is_exempt() {
    let repo = Repository::new();
    let registry = format!(
        "registry+{}://github.com/rust-lang/crates.io-index",
        "https"
    );
    for (source, expected) in [(registry.clone(), 0), (format!("{registry}/extra"), 1)] {
        repo.write(
            "Cargo.lock",
            &format!("[[package]]\nname = 'fixture'\nsource = {source:?}\n"),
        );
        assert_eq!(
            literals::scan(repo.root(), &[PathBuf::from("Cargo.lock")], "")
                .unwrap()
                .len(),
            expected
        );
    }
    repo.write(
        "Cargo.lock",
        &format!("# {registry}\n[[package]]\nname = 'fixture'\n"),
    );
    assert_eq!(
        literals::scan(repo.root(), &[PathBuf::from("Cargo.lock")], "")
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn compilable_block_local_modules_keep_distinct_namespaces() {
    for (name, source, expected) in [
        (
            "local and module-level names",
            include_str!("../tests/fixtures/structure/modules-local-shadow.rs"),
            0,
        ),
        (
            "names in separate blocks",
            include_str!("../tests/fixtures/structure/modules-local-blocks.rs"),
            0,
        ),
        (
            "cycle inside a local module",
            include_str!("../tests/fixtures/structure/modules-local-cycle.rs"),
            2,
        ),
    ] {
        let repo = Repository::new();
        repo.write("src/lib.rs", source);
        let output = Command::new("cargo")
            .args(["check", "--offline", "--locked"])
            .current_dir(repo.root())
            .output()
            .unwrap();
        assert!(output.status.success(), "{name} must compile");
        let measured = measure(
            repo.root(),
            &history::working_files(repo.root()).unwrap(),
            &history::repository_url(repo.root()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            measured.counts.module_cycles, expected,
            "{name}: {:?}",
            measured.findings
        );
    }
}

#[test]
fn new_counts_and_issue_linked_allowances_work_end_to_end() {
    for (metric, files, amount) in [
        (
            "module_cycles",
            vec![(
                "src/lib.rs",
                include_str!("../tests/fixtures/structure/modules-fail.rs"),
            )],
            3,
        ),
        (
            "source_copies",
            vec![("copy.rs", "pub fn value() -> u8 { 1 }\n")],
            1,
        ),
        (
            "settings_in_code",
            vec![("src/lib.rs", "const RETRY_COUNT: u8 = 3;\n")],
            1,
        ),
    ] {
        let repo = Repository::new();
        for (path, text) in files {
            repo.write(path, text);
        }
        let base = repo.git(&["rev-parse", "HEAD"]);
        assert!(
            check(repo.root(), Some(&base))
                .unwrap_err()
                .contains(metric)
        );
        let repository = history::repository_url(repo.root()).unwrap();
        // The issue URL also contributes one instance literal.
        repo.write("structure/allowances.toml", &format!(
            "[[allowances]]\nid = 'debt'\nmetric = '{metric}'\namount = {amount}\nreason = 'temporary fixture debt'\nissue = '{repository}/issues/8'\n\n[[allowances]]\nid = 'issue-links'\nmetric = 'instance_literals'\namount = 2\nreason = 'fixture allowance issue links'\nissue = '{repository}/issues/8'\n"));
        assert!(check(repo.root(), Some(&base)).is_ok(), "{metric}");
        let head = repo.commit();
        assert!(
            check(repo.root(), Some(&base)).is_ok(),
            "committed {metric}"
        );
        assert!(
            check(repo.root(), Some(&head)).is_ok(),
            "historical {metric}"
        );
    }
}

#[test]
fn source_inventory_skips_fixtures_docs_non_rust_and_empty_files() {
    let repo = Repository::new();
    let source = "const RETRY_COUNT: u8 = 3;\n";
    for path in ["xtask/tests/fixtures/copy.rs", "docs/copy.rs", "input.txt"] {
        repo.write(path, source);
    }
    for path in ["empty.rs", "other-empty.rs"] {
        repo.write(path, "");
    }
    assert!(check(repo.root(), Some("HEAD")).is_ok());
    repo.write("untracked.rs", source);
    assert!(
        check(repo.root(), Some("HEAD"))
            .unwrap_err()
            .contains("settings_in_code")
    );
    repo.write("untracked.rs", "pub fn independent() {}\n");
    repo.git(&["add", "."]);
    assert!(check(repo.root(), Some("HEAD")).is_ok());
}

#[test]
fn tuple_field_chains_are_code_but_bare_macro_addresses_still_count() {
    let repo = Repository::new();
    let fields = ".0".repeat(4);
    let numeric_fields = fields.strip_prefix('.').unwrap();
    repo.write(
        "src/lib.rs",
        &format!(
            r#"
pub fn fields(value: ((((u8,),),),)) -> u8 {{ value{fields} }}
pub fn parenthesized(value: ((((u8,),),),)) -> u8 {{ (value){fields} }}
pub fn spaced(value: ((((u8,),),),)) -> u8 {{ (value) . /* field */ {numeric_fields} }}
pub const MACRO: &str = stringify!((value){fields});
"#
        ),
    );
    let output = Command::new("cargo")
        .args(["check", "--offline", "--locked"])
        .current_dir(repo.root())
        .output()
        .unwrap();
    assert!(output.status.success(), "tuple field controls must compile");
    assert!(check(repo.root(), Some("HEAD")).is_ok());
    let address = Ipv4Addr::new(10, 20, 30, 40);
    repo.write(
        "src/lib.rs",
        &format!(
            r#"
#[doc = stringify!({address})]
pub const BARE: &str = stringify!({address});
pub const TEXT: &str = "(value){fields}";
// (value){fields}
"#
        ),
    );
    let findings = literals::scan(repo.root(), &[PathBuf::from("src/lib.rs")], "").unwrap();
    assert_eq!(findings.len(), 4, "{findings:?}");
    assert!(findings.iter().all(|f| f.ends_with("IPv4 literal")));
}

#[test]
fn version_context_exempts_metadata_but_not_standalone_addresses() {
    let repo = Repository::new();
    let address = Ipv4Addr::new(10, 20, 30, 40);
    for version in [
        format!("1.2.3+{address}"),
        format!("1.2.3-beta+build.{address}"),
        format!("1.2.3-{address}"),
        format!("1.2.3-{address}+build"),
    ] {
        for (file, text) in [
            ("version.txt", format!("version = {version}\n")),
            (
                "version.rs",
                format!("// {version}\nconst VERSION: &str = {version:?};\n"),
            ),
        ] {
            repo.write(file, &text);
            let findings = literals::scan(repo.root(), &[PathBuf::from(file)], "").unwrap();
            assert!(findings.is_empty(), "{file}: {findings:?}");
        }
    }
    for text in [
        address.to_string(),
        format!("+{address}"),
        format!("endpoint+{address}"),
        format!("1.2+{address}"),
        format!("{address}+{address}"),
        format!("1.2.3+ {address}"),
        format!("1.2.3+{address}+extra"),
    ] {
        assert!(!literals::in_line(&text).is_empty(), "{text}");
    }
}

#[cfg(unix)]
#[test]
fn exclusions_do_not_allow_symlinks_in_current_or_historical_inventory() {
    use std::os::unix::fs::symlink;
    let repo = Repository::new();
    for file in [
        "docs/link.txt",
        "NOTES.md",
        "LICENSE",
        "xtask/tests/fixtures/link.rs",
    ] {
        let path = repo.root().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A dangling link must fail too, without trying to read its target.
        symlink("missing-target", &path).unwrap();
        repo.git(&["add", file]);
        let files = history::working_files(repo.root()).unwrap();
        assert!(
            literals::scan(repo.root(), &files, "")
                .unwrap_err()
                .contains("symlinks"),
            "{file}"
        );
        assert!(
            check(repo.root(), Some("HEAD"))
                .unwrap_err()
                .contains("symlinks")
        );
        let head = repo.commit();
        assert!(
            history::Snapshot::create(repo.root(), &head)
                .err()
                .unwrap()
                .contains("unsupported Git tree entry")
        );
        repo.git(&["rm", file]);
        repo.commit();
        assert!(check(repo.root(), Some("HEAD")).is_ok());
    }
}
