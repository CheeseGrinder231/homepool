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

Core holds pure types and logic, with no I/O. Adapters live under
`crates/adapters/`, including the Docker engine adapter. Applications include the CLI and any future
hub. The `xtask` development tool belongs to app. A missing assignment or an unknown
layer fails the check. The three allowed directions are fixed by the checker, so
editing the configuration cannot silently loosen the rules.

Run `cargo xtask structure` before pushing. The check reads
`cargo metadata --format-version 1 --all-features --locked`. It includes development,
build, optional, renamed, and target-specific dependencies, and counts each forbidden
workspace dependency edge once. Cargo already rejects crate dependency cycles.

## Instance-specific literals

The check scans all tracked text files, including Cargo configuration and files
consumed by `include_str!`, `include_bytes!`, and `include!`, regardless of extension.
Text means valid UTF-8 without NUL bytes; other files are treated as binary and
skipped. Local checks also include non-ignored untracked files before staging.
The explicit exclusions are `docs/`, `*.md`, files named `LICENSE`, and
`xtask/tests/fixtures/`. Generated files normally stay outside the Git inventory;
tracked files under `target/` are still scanned. Symlinks fail closed. Historical
scans use the exact Git tree's inventory, blob contents, and executable modes;
export attributes and checkout filters cannot omit or rewrite the evidence.

Each finding reports its relative file, line, column, and category. The categories
are IPv4 and IPv6 literals outside the RFC 5737 and RFC 3849 documentation ranges,
absolute home-directory paths (Unix account homes, the root account's home, and
Windows drive-qualified user profiles), email addresses, and HTTP or HTTPS URL
literals with case-insensitive schemes. Sentence punctuation does not hide IPv6.
IPv4 dotted-quads are checked across full Rust source text, including code and
bare macro input, so `stringify!` and `doc = stringify!` cannot hide contiguous
addresses. Rust sources are also tokenized with `ra-ap-rustc_lexer`: IPv6, home
paths, email addresses, and URLs are checked only in string-literal contents
(including byte, C, and raw strings), comments, and shebang text. Literal suffixes
are consumed with their token. Other text files use the plain-text scanner for
all categories. Home paths must begin an absolute path token; relative directory
components do not count.

Only the exact canonical HTTPS repository URL derived from origin is exempt, and
only as the value of `package.repository` or `workspace.package.repository` in a
`Cargo.toml`. Homepage URLs, dependency URLs, comments, source strings, and URLs
with a different path or suffix still count. The exact generated crates.io registry
source field in the root `Cargo.lock` is also exempt; comments and altered source
URLs are not. There is no general URL or domain allowlist. Other matching URLs,
including issue links in allowance entries, count.

The threat model is **accidental leaks in committed text**. This is an unexpanded
source-text check, not a constant evaluator: it does not decode escapes, expand
macros, or evaluate constants. Deliberate reconstruction with `concat!`, split
fragments, escape sequences, computed values, or other compile-time/runtime
assembly is out of scope and must be caught by review. Binary files and inputs
outside the tracked/non-ignored local inventory are not inspected. The documented
exclusions remain excluded even when referenced by an include macro; review must
check those uses.

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
example, one new literal plus its allowance's issue URL needs one
`instance_literals` entry with amount 2. The initial change needs no allowances.

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
