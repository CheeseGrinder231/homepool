# Local hub

`homepool hub` serves placement and member lifecycle requests on one owner-only
Unix socket. It opens no network listener. The member file and Docker inventories
are the source of truth; there is no hub database or persistent reservation ledger.
`homepool status` continues to show declarations without contacting the hub.

## Setup

Use a private member file as described in [members.md](members.md). Each compute
member needs an explicit Docker endpoint, for example:

```toml
version = 1
[[members]]
id = 1
tier = "core"
state = "active"
[members.capabilities.compute]
cpu_millicores = 1000
memory_bytes = 268435456
sandbox_slots = 3
[members.docker]
socket = "/run/member/docker.sock"
```

For SSH, replace the entire `members.docker` table with:

```toml
[members.docker]
host = "192.0.2.1"
port = 22
remote_socket = "/run/member/docker.sock"
```

`port` and `remote_socket` are optional. Authentication uses existing SSH
configuration. Endpoints contain no credentials and are omitted from status JSON.
An endpoint is optional for compatibility with existing member files; a member
without one remains closed to placements because its inventory is unknown.

The hub requires a regular member file owned by the current user, mode `0600`,
and an immediate parent directory owned by that user, mode `0700`. It holds an
exclusive sidecar lock for the lifetime of the process; use one hub per member
file. Keep all private files outside the repository. Do not edit the file while
lifecycle requests are running; external editors do not participate in this lock.

```sh
homepool --config "$HOMEPOOL_CONFIG" --socket "$HOMEPOOL_SOCKET" hub
homepool --socket "$HOMEPOOL_SOCKET" inventory --json
```

Config selection is unchanged. Socket selection takes the first supplied source:
`--socket FILE`, `HOMEPOOL_SOCKET`, then `$XDG_RUNTIME_DIR/homepool/hub.sock`.
Explicit and environment paths may be relative to the working directory. Empty
selections fail; the XDG directory must be absolute and nonempty. There is no home
fallback. New socket parent directories use `0700`; existing immediate parents
must already have that mode and the current owner. The socket is created with
owner-only permissions from bind, restricted to `0600`, and checked by the hub and
clients for its type, owner and permissions. Symlinks are rejected.

The hub never unlinks an existing socket path on startup. After an uncatchable
termination, first establish privately that no hub is running, then remove the
stale socket before restarting. SIGINT and SIGTERM cooperatively cancel active
Docker operations, terminate their process groups, reap the direct children,
close API streams, remove the socket and exit successfully. Cancellation actions
are scoped to the serving loop. On return, termination signals use their default
behavior, including while reporting a failure. Shutdown leaves
running sandboxes in place. Docker operations have the adapter's 60-second
end-to-end deadline; see [docker.md](docker.md) for unknown mutation outcomes.

## Placement and release

`place --json` reads one placement specification from stdin. CPU and memory must
be positive; CPU must be a multiple of 10 millicores. The engine can impose a
larger minimum memory limit. `sandbox_id` is nonsecret and descriptive. The hub
rejects a placement whose identity is already in its observed inventories, so
inspect and release that work before intentionally reusing the identity.

```sh
printf '%s\n' '{"sandbox_id":"example-job","image":"alpine:3.22","cpu_millicores":200,"memory_bytes":33554432,"command":["sleep","300"]}' |
  homepool --socket "$HOMEPOOL_SOCKET" place --json
homepool --socket "$HOMEPOOL_SOCKET" release "$SANDBOX_HANDLE"
```

Optional `preferences` is an array of numeric member IDs; the core placement
rules apply. Optional `command` is a string array; omission uses the image default.
Optional `environment` is a map of explicit strings, sent over Docker stdin. Keep
secret values out of shell history, process arguments, images and identities;
use a private producer for secret-bearing placement input. Mounts are not exposed
by this small API. Placement returns `{"member":1,"handle":"1:<full-container-id>"}`.
The handle is the member ID and full Docker container ID, independent of inventory
order, process lifetime and the sandbox label. `release` stops and removes that
exact observed managed container, including when the member is draining or its
budget has shrunk. It does not remove volumes. Already absent handles return an
error; a complete `inventory` confirms absence.

Each request refreshes inventories before acting. Every member begins unreachable;
only a complete, bounded inventory successfully imported into the declared budget
can restore its declared admission state. Stopped and created containers retain
their allocations until explicitly released. Unknown limits, failed inspection,
unavailable engines, and rejected imports keep admission closed. Known observed
handles are retained even when their imports fail. Free budgets on a closed member
are zero; the inventory response distinguishes `inventory_complete: false` from
an accounted draining member. A failed inventory preserves prior observations as
stale handles, never as evidence of free capacity. A fresh process uses only fresh
member inventories to reconstruct its map. Numeric core reservation IDs are
deterministic hashes of the full handle with collision detection; they are never
external handles. A collision closes the affected member.

After an uncertain start or release, the hub never retries automatically. Inspect
`inventory` and reconcile the observed handles before submitting more work. A
successful listing describes current engine state, not completion of a remote
mutation still being processed. In particular, do not blindly repeat a request
when a response is lost or an engine was unreachable. Keep the member draining
until private inspection settles a delayed mutation. Duplicate labels from
external clients are visible as separate handles and remain separately accounted.

## Member lifecycle

```sh
homepool --socket "$HOMEPOOL_SOCKET" add "$MEMBER_ADDITION"
homepool --socket "$HOMEPOOL_SOCKET" drain 1
homepool --socket "$HOMEPOOL_SOCKET" remove 1
```

`add FILE` reads a private `0600` regular file containing the same version-1 schema
with exactly one member. Duplicate IDs, unknown fields and invalid endpoints fail
without writing. `drain` persists the declared draining state across restarts and
keeps existing sandboxes running. `remove` requires a draining member with a
complete empty inventory; finish or release its work first. An unavailable
inventory cannot prove that removal is safe.

Updates rewrite the member file in canonical ID order (comments and original
formatting are not preserved), validate the entire replacement, write a `0600`
file alongside it, sync it, atomically rename it, then sync its parent directory.
A directory-sync failure reports that replacement occurred but durability could
not be confirmed. Member contents, endpoints, placement input and Docker raw
output are never written to hub logs. Inventory output contains private logical
identities and handles; keep it private.

## Local API version 1

The CLI uses newline-delimited JSON over the same Unix socket. Each connection
carries exactly one request and one response, at most 1 MiB per message. Reads and
writes are nonblocking, bounded to 120 seconds and cancelled during shutdown.
Requests are serialized; there is no job queue or scheduler beyond placement.
For example, an inventory request is `{"op":"inventory"}`; placement is
`{"op":"place",...placement fields...}`. Other operations are
`{"op":"release","handle":"..."}`, `{"op":"drain","member":1}`,
`{"op":"remove","member":1}`, and `{"op":"add","document":"..."}`.
Unknown fields and operations fail. Responses are
`{"version":1,"result":{...},"error":null}` or
`{"version":1,"result":null,"error":"..."}`. CLI commands unwrap `result`;
errors exit `1` without success JSON. Socket permissions are the authorization
boundary; there is no network endpoint or additional authentication protocol.
A disconnected client must reconcile before retrying a mutation.

[The example systemd user unit](examples/homepool-hub.service) uses XDG runtime
paths and an explicit private member file. Copy it into the user unit directory
and adjust only the private local copy as needed; the binary must be on the
service's executable search path. Enable it with `systemctl --user enable --now
homepool-hub.service` after installing the binary and private configuration.

## Tests and resource measurements

`cargo test --workspace` runs process-level restart, drain, shrink recovery,
uncertain-start, private-file and SIGINT/SIGTERM tests against a persistent fake
engine. The restart test stops one hub process, launches a different process with
no old map, checks differently sized running sandbox inventory and occupied
capacity, rejects duplicate work, drains across restart and releases old handles.
The signal tests cancel stalled Docker wrappers and check transport termination.
The adapter additionally tests child reaping and closure of inherited pipes.

With a disposable Docker engine and `alpine:3.22` pulled, run:

```sh
cargo test -p homepool-cli --test hub -- --ignored --nocapture
```

This repeats the actual-process restart and drain test with real running
containers. `HOMEPOOL_TEST_DOCKER_SOCKET` optionally selects a local engine socket.
Tests use `TMPDIR` for private scratch and clean up test-created work. Procfs
samples sum CPU seconds across the three hub processes and report the latest
resident memory and greatest sampled process high-water mark during the test; the PR records measured values and test conditions. These are
process measurements, excluding Docker, SSH and workload resource consumption.
