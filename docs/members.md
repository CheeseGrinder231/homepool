# Private member file

`homepool status` reads one UTF-8 TOML file and displays its declared members,
tiers, states, capabilities and resource budgets. By default it performs no network
probes or runtime discovery. States and budgets reflect the file, not observed
health, current usage or measured free capacity. The optional Garage probe below
adds live storage counters without changing these declarations.

Copy [the placeholder example](examples/members.toml) to a private location
outside this repository, restrict its permissions to its owner, then edit the copy.
Never commit a real member file. Status output and validation errors can contain
private values and paths; keep them private too. The command only reads the file
and does not create it, change its permissions or write its contents elsewhere.

## File selection

The first supplied source wins:

1. `--config FILE`, before or after `status`.
2. The `HOMEPOOL_CONFIG` environment variable.
3. `$XDG_CONFIG_HOME/homepool/members.toml`.

For example, from the workspace root:

```sh
cargo run -p homepool-cli -- --config "$HOMEPOOL_CONFIG" status
cargo run -p homepool-cli -- status --config "$HOMEPOOL_CONFIG" --json
```

Explicit paths and `HOMEPOOL_CONFIG` may be absolute or relative to the current
working directory. Shell expansion happens in the shell, not in the file reader.
`XDG_CONFIG_HOME` must be set to an absolute, nonempty directory path when the
third source is used. If it is unset, the command asks for an explicit path or
`HOMEPOOL_CONFIG`; there is no implicit home-directory fallback. An empty selected
path is an error. A selected file that is missing, unreadable or invalid fails
without trying a lower-priority source.

## Schema version 1

The root requires `version = 1` and a `members` array. `members = []` is a valid
empty pool. Use `[[members]]` for each member, or equivalent inline TOML tables.
Unknown fields are errors at every level; future incompatible schemas require a
new version.

| Field | Type and meaning |
| --- | --- |
| `version` | Required integer, exactly `1`. |
| `members[].id` | Required unique unsigned 64-bit integer, an opaque pool-local ID. |
| `members[].tier` | Required string: `core` (expected to remain on) or `burst` (may come and go). |
| `members[].state` | Required string: `active`, `draining` or `unreachable`. |
| `members[].capabilities` | Required table; may be empty. |
| `members[].capabilities.storage_bytes` | Optional unsigned 64-bit integer; defaults to `0` (no shared storage contribution). |
| `members[].capabilities.compute` | Optional table; omission means no compute contribution. |
| `members[].capabilities.compute.cpu_millicores` | Required unsigned 64-bit integer when compute is present; 1,000 millicores is one CPU core. |
| `members[].capabilities.compute.memory_bytes` | Required unsigned 64-bit integer when compute is present. |
| `members[].capabilities.compute.sandbox_slots` | Required unsigned 32-bit integer when compute is present; maximum simultaneous sandboxes. |
| `members[].capabilities.compute.profiles` | Optional table of named launch profiles; defaults to empty. |

All resource amounts are integers, never floats or unit strings. Zero is valid.
Unsigned 64-bit fields accept `0` through `18446744073709551615`; sandbox slots
accept `0` through `4294967295`. TOML integer notation, including underscores and
hexadecimal values, is supported. Tier and state names are case-sensitive.
Compute budgets describe resources dedicated to this pool. Storage is descriptive
and does not add sandbox capacity. Draining and unreachable members remain visible
with their declared budgets. No reservations or work are created by status.
The schema currently needs neither names nor network addresses.

## Diagnostics

Failures exit with code `1`, except signal cancellation described below, and write
a diagnostic to stderr. Failures before output
begins leave stdout empty, including with `--json`. Output failures, such as a reader
closing a pipe, may leave partial output (including incomplete JSON) on stdout.
Successful status exits with code `0`.
Validation diagnostics have the form:

```text
members.toml:12: members[0].capabilities.compute.sandbox_slots: expected an integer from 0 to 4294967295
```

Lines are one-based and member indexes are zero-based in input order. Invalid
values point to the value; unknown fields point to the key. A missing field points
to its enclosing table's start (line 1 for missing root fields). Duplicate member
IDs point to the later ID. Invalid TOML reports the parser's line with the field
`<document>` because no valid schema field may exist yet. File I/O and UTF-8
failures report line 1 and `<file>`. Path-selection and argument errors identify
the relevant option or environment variable before a file is selected.

## Status JSON version 1

`homepool status --json` writes one JSON object followed by a newline. The top-level
`version` versions this output contract independently of the member-file schema.
Members are ordered by ascending numeric ID, independent of input order.
Keys, types, enum spellings and null handling are
stable for version 1; consumers should parse JSON rather than depend on whitespace
or object key order. Without the live option below, no paths, timestamps or
transient runtime data are included.

For a single member with ID 1 and empty capabilities:

```json
{"version":1,"members":[{"id":1,"tier":"core","state":"active","capabilities":{"compute":null,"storage_bytes":0}}]}
```

When compute is present, `compute` is an object with exactly `cpu_millicores`,
`memory_bytes` and `sandbox_slots`, each a JSON integer in the ranges above.
When profiles are declared, compute also includes `profiles`, a sorted array of
names only; an empty array is omitted. Profile settings and paths never appear
in either human or JSON status output.
An empty pool is `{"version":1,"members":[]}`. Consumers must preserve large integer
values exactly; some JSON runtimes need an integer-aware parser beyond their
native floating-point range. Default JSON reports the same declarations as the human
output; `compute: null` distinguishes no compute from an all-zero compute budget.

## Optional live Garage storage

```sh
cargo run -p homepool-cli -- --config "$HOMEPOOL_CONFIG" status --garage-config "$GARAGE_CONFIG"
cargo run -p homepool-cli -- --config "$HOMEPOOL_CONFIG" status --garage-config "$GARAGE_CONFIG" --json
```

`--garage-config FILE` selects the existing Garage program's private configuration.
It may occur before or after `status`, once only. It has no implicit environment
fallback. The installed `garage` executable must be available on `PATH`.
Only this explicit option performs live probes, through Garage's configured RPC
connection. No admin token or S3 credential is accepted as a command argument.
See [Garage setup and consumer isolation](garage.md).

During a live Garage probe, Ctrl-C (`SIGINT`) and `SIGTERM` request cooperative
cancellation. The CLI stops the local Garage process group, including wrapper
descendants, and reaps its direct child before exiting with code `130` for SIGINT
or `143` for SIGTERM. A pending probe produces no stdout and reports the fixed
sanitized cancellation diagnostic. Cancellation of a mutation has the same unknown
outcome as a deadline: inspect Garage privately before retrying; no operation is
automatically retried. Uncatchable termination cannot perform this cleanup.

After the probe returns and child cleanup is complete, SIGINT and SIGTERM use
ordinary Unix signal termination, including when status output is blocked on a
stalled pipe. Shells report `130` or `143`, respectively; partial output may remain.

This is an additive, opt-in extension of status JSON version 1: with the option,
the top-level object also contains `storage`. Without the option, this field is
omitted and the previous JSON shape is unchanged. The member-file schema is unchanged.
The `storage` object has four unsigned 64-bit integer fields:

| Field | Meaning |
| --- | --- |
| `buckets` | Number of buckets with a reserved `homepool-consumer-ID` global alias. |
| `objects` | Sum of completed object counts in those buckets. |
| `used_bytes` | Sum of logical completed-object bytes. |
| `unfinished_upload_bytes` | Sum of unfinished multipart-upload bytes, separate from `used_bytes`. |

Each bucket is counted once even if it has multiple aliases. Other buckets are
excluded. Counters may lag recent writes/deletes and do not measure physical disk
usage, replication overhead or remaining member capacity. An empty store returns
zero counters. Other probe failures, malformed counters or overflowing sums exit `1`,
leaves stdout empty and reports a fixed diagnostic without subprocess output,
configuration paths, credentials or server error bodies. No credentials or node
addresses are included in successful status output either.

Human output includes the single-copy warning: **a single-node layout keeps a
single copy; it is not a backup.** The same warning applies when using JSON.

## Optional Docker endpoints and lifecycle

Members may also contain a strict `docker` table: either `socket` (an absolute
local Unix socket path), or `host` with optional `port` and `remote_socket` for SSH.
These private values are validated and omitted from status output. Existing
files without endpoints retain the same declaration-only status behavior.
See [hub.md](hub.md) for endpoint examples, private-file requirements, atomic
`add`/`remove` updates and persistent `drain` behavior.

## Named sandbox profiles

The owner may define typed launch settings under
`members[].capabilities.compute.profiles.<name>`. Existing version 1 files remain
valid. Profile and volume suffix names start with an ASCII letter or digit and
contain only ASCII letters, digits, dash, underscore and dot. Names are
case-sensitive. Volume suffixes have at most 96 bytes so the complete name fits
a 255-byte local volume directory component even with the longest sandbox and
member identities. A profile can be empty; omitted booleans default to `false` and
omitted mount arrays default to empty.

```toml
[members.capabilities.compute.profiles.runner]
privileged = true
pids_limit = 256
shm_bytes = 67108864
memory_swap_bytes = 536870912
tmpfs = [{ target = "/scratch", options = "rw,size=64m,mode=1777" }]
volumes = [{ name = "cache", target = "/cache", read_only = false }]
binds = [{ source = "/srv/runner-input", target = "/input", read_only = true }]
```

`pids_limit`, `shm_bytes` and `memory_swap_bytes` are optional positive integers
up to `9223372036854775807`, Docker's signed integer limit. Swap is the total
memory-plus-swap limit and must be at least the placement's `memory_bytes`.
`tmpfs` entries require `target` and `options`. Options accept only ASCII letters,
digits, comma, equals, dot, underscore and dash. An empty options string uses
Docker's defaults. Volume entries require `name` and `target`; bind entries require
`source` and `target`. `read_only` defaults to `false` for both.

All mount paths must be absolute and contain no commas, double quotes or control
characters. Tmpfs targets additionally reject colon, which separates options.
Bind sources refer to that member's engine filesystem. Unknown fields, bad names,
paths, options, and zero or out-of-range limits fail when the file is read,
including through `add`, before running Docker. Docker still validates supported
options and engine-specific limits at creation.

Anyone who can reach the owner-only hub socket can select a profile and start
privileged containers on a member offering it. On a rootless engine this stays
inside the engine's user namespace. On a rootful engine it is effectively root
on the member. Define privileged profiles only where that access is intended.

Each volume is named `homepool-<member-id>-<sandbox-id>-<name>`, using the existing
deterministic container name and the configured suffix. Different sandbox
identities get separate volumes. `release` never deletes volumes. Reusing the
same identity on the same member after release reuses its volume; the owner
manages retained data and removes volumes separately.
