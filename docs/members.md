# Private member file

`homepool status` reads one UTF-8 TOML file and displays its declared members,
tiers, states, capabilities and resource budgets. It performs no network probes
or runtime discovery. States and budgets reflect the file, not observed health,
current usage or measured free capacity.

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

All resource amounts are integers, never floats or unit strings. Zero is valid.
Unsigned 64-bit fields accept `0` through `18446744073709551615`; sandbox slots
accept `0` through `4294967295`. TOML integer notation, including underscores and
hexadecimal values, is supported. Tier and state names are case-sensitive.
Compute budgets describe resources dedicated to this pool. Storage is descriptive
and does not add sandbox capacity. Draining and unreachable members remain visible
with their declared budgets. No reservations or work are created by status.
The schema currently needs neither names nor network addresses.

## Diagnostics

Failures exit with code `1` and write a diagnostic to stderr. Failures before output
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
or object key order. No paths, timestamps or transient runtime data are included.

For a single member with ID 1 and empty capabilities:

```json
{"version":1,"members":[{"id":1,"tier":"core","state":"active","capabilities":{"compute":null,"storage_bytes":0}}]}
```

When compute is present, `compute` is an object with exactly `cpu_millicores`,
`memory_bytes` and `sandbox_slots`, each a JSON integer in the ranges above.
An empty pool is `{"version":1,"members":[]}`. Consumers must preserve large integer
values exactly; some JSON runtimes need an integer-aware parser beyond their
native floating-point range. JSON reports the same declarations as the human
output; `compute: null` distinguishes no compute from an all-zero compute budget.
