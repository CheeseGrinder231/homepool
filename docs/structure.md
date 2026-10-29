# Structure checks

The workspace uses Rust 1.97.1 and edition 2024. Every crate inherits the workspace
lints: unsafe code is forbidden and Clippy's `all` group is denied.

## Dependency layers

`structure/layers.toml` assigns every workspace crate a layer:

| Layer | May depend on internally |
| --- | --- |
| core | Nothing |
| adapter | Core only |
| app | Core and adapters |

Core holds pure types and logic, with no I/O. Adapters will live under
`crates/adapters/`; none are needed yet. Applications include the CLI and any future
hub. The `xtask` development tool belongs to app. A missing assignment or an unknown
layer fails the check. The three allowed directions are fixed by the checker, so
editing the configuration cannot silently loosen the rules.

Run `cargo xtask structure` before pushing. The check reads
`cargo metadata --format-version 1 --all-features --locked`. It includes development,
build, optional, renamed, and target-specific dependencies, and counts each forbidden
workspace dependency edge once. Cargo already rejects crate dependency cycles.

## Instance-specific literals

The check scans every tracked Rust source and `Cargo.toml` in the repository,
including root packages, build scripts, integration tests, and relocated modules.
Local checks also include non-ignored untracked sources so newly added files are
checked before staging. The explicit exclusions are `xtask/tests/fixtures/`, Git
metadata, and generated `target/` directories. Symlinks fail closed rather than
allowing sources to escape the scan. Historical scans use the exact Git tree's file
inventory, blob contents, and executable file modes; export attributes and checkout
filters cannot omit or rewrite the evidence.

Each finding reports its relative file, line, column, and category. The categories
are IPv4 and IPv6 literals outside the RFC 5737 and RFC 3849 documentation ranges,
absolute home-directory paths (Unix account homes, the root account's home, and
Windows drive-qualified user profiles), email addresses, and HTTP or HTTPS URL
literals with case-insensitive schemes. Sentence punctuation does not hide IPv6.
Rust sources are tokenized with `ra-ap-rustc_lexer`. Only string-literal contents
(including byte, C, and raw strings), comment text (including nested comments), and
the shebang line are scanned. Literal suffixes are consumed with their token;
identifiers, paths, numbers, and character literals are never scanned. Cargo
manifests are scanned as text. Home paths must begin an absolute path token;
relative directory components do not count.

Only the exact canonical HTTPS repository URL derived from origin is exempt, and
only as the value of `package.repository` or `workspace.package.repository` in a
`Cargo.toml`. Homepage URLs, dependency URLs, comments, source strings, and URLs
with a different path or suffix still count. No general URL or domain allowlist is
provided. The scanner checks literal text; review still enforces public-repository
hygiene for values assembled at runtime.
Do not publish machine names, hostnames, home-directory paths, usernames, emails,
private repository names, internal issue details, or information about a contributor's
host. Use generic product language and documentation addresses in examples.

## The ratchet

`structure/baseline.toml` starts all three counts at zero:

- `layer_violations`: forbidden internal dependency edges.
- `instance_literals`: the source findings described above.
- `allowance_count`: entries in `structure/allowances.toml`.

The checker compares the working tree with a Git base snapshot. On a branch, the
default is the merge base with `origin/main`. When HEAD matches that tracking ref,
a dirty working tree or index is compared with HEAD; a clean tree checks the
committed main change against its first parent. A true root commit and an explicit
all-zero before SHA use an initial-zero snapshot. Other invalid revisions fail.
CI supplies the PR base commit or the push's previous commit explicitly using
`cargo xtask structure --base REF`. Full Git history is required. A base without a
Cargo workspace is treated as the initial zero state. Historical workspace checks
use the historical manifests and lockfile, not today's dependency graph.

Baselines may only fall. Counts above a baseline require allowances covering the
excess. Any increase above both the base count and the current baseline must be
covered by **new allowance entries in the same change**; unused capacity in old
allowances cannot fund new debt. If a count falls below its baseline, lower the
baseline in that PR. Otherwise the check reports
`ratchet: lower <metric> baseline to N`.

Each allowance needs a unique, nonempty `id`, a `metric` of `instance_literals` or
`layer_violations`, a positive integer `amount`, a nonempty `reason`, and an `issue`
URL pointing to an issue in this repository. The checker derives the repository from the Git origin. Existing entries
are immutable relative to the base, including their reasons and issue links. Delete
entries when retired. Allowance sums cannot overflow.

Every allowance contributes one to `allowance_count` and funds exactly that one
entry automatically. Its `amount` funds only its named debt metric. New entries
supply their own count coverage; old spare capacity cannot fund new debt, and no
separate allowance-for-allowances entries are accepted. The count is still reported,
baselines still only fall, and an existing entry still cannot be edited. For
example, one new literal finding needs one issue-linked `instance_literals` entry
with amount 1. The initial change needs no allowances.

## Local and CI checks

Run these from the workspace root:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo xtask structure
cargo deny check licenses bans sources
```

The required-check candidates keep the stable job names `fmt`, `clippy`, `test`,
`structure`, and `cargo-deny`. CI runs for pull requests and pushes to main on
GitHub-hosted runners, with read-only contents permissions and SHA-pinned actions.

Dependency licenses are restricted to Apache-2.0, MIT, BSD-2-Clause, BSD-3-Clause,
ISC, Unicode-3.0, and Zlib. Copyleft-only dependencies are not allowed. Unknown
registries and Git sources are denied; crates.io is the default allowed registry.
Wildcard version requirements are also denied; duplicate versions produce warnings.
The separate weekly advisory workflow is non-blocking and can be run manually.

The tokenizer's Unicode tables must agree. The lockfile selects compatible
Unicode 17 releases of `unicode-ident` and `unicode-properties`; keep their Unicode
versions aligned when updating dependencies.
