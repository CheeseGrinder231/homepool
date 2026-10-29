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

The check scans Rust sources under `crates/` and `xtask/src/`, and every `Cargo.toml`.
It excludes `xtask/tests/fixtures/`, Git metadata, and generated `target/` directories.
Symlinks fail closed rather than allowing sources to escape the scan.

Each finding reports its relative file, line, column, and category. The categories
are IPv4 and IPv6 literals outside the RFC 5737 and RFC 3849 documentation ranges,
absolute home-directory paths, email addresses, and HTTP or HTTPS URL literals.
Comments and strings are included. The scanner is a literal check, not a detector
for values assembled at runtime; review still enforces public-repository hygiene.
Do not publish machine names, hostnames, home-directory paths, usernames, emails,
private repository names, internal issue details, or information about a contributor's
host. Use generic product language and documentation addresses in examples.

## The ratchet

`structure/baseline.toml` starts all three counts at zero:

- `layer_violations`: forbidden internal dependency edges.
- `instance_literals`: the source findings described above.
- `allowance_count`: entries in `structure/allowances.toml`.

The checker compares the working tree with a Git base snapshot. On a branch, the
default is the merge base with `origin/main`. On main, it checks the last commit.
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

Each allowance needs a unique, nonempty `id`, a known `metric`, a positive integer
`amount`, a nonempty `reason`, and an `issue` URL pointing to an issue in this
repository. The checker derives the repository from the Git origin. Existing entries
are immutable relative to the base, including their reasons and issue links. Delete
entries when retired. Allowance sums cannot overflow.

Allowances themselves count as debt. For example, granting one literal finding needs
two entries: one for `instance_literals` with amount 1, and one for `allowance_count`
with amount 2, covering both new entries. Both need reasons and issue links. This
makes the cost of an exception visible instead of allowing an unlimited exception
list. The initial change needs no allowances.

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
