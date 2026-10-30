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
Explicit and environment paths may be relative to the working directory. Unix
socket paths have a short platform-dependent limit of about 100 bytes; a relative
path from a nearby working directory avoids the limit on a long absolute path. Empty
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
larger minimum memory limit. The caller-chosen, nonsecret `sandbox_id` is the
idempotency key. The adapter uses a deterministic container name containing the
member ID and the exact sandbox identity, so Docker refuses a second create of
the same identity on that member with typed `IdentityConflict`. This applies to
running, stopped and created containers while they exist; deleting the container
removes its engine-owned name. There is no hub database or identity journal.

```sh
printf '%s\n' '{"sandbox_id":"example-job","image":"alpine:3.22","cpu_millicores":200,"memory_bytes":33554432,"command":["sleep","300"]}' |
  homepool --socket "$HOMEPOOL_SOCKET" place --json
homepool --socket "$HOMEPOOL_SOCKET" release "$SANDBOX_HANDLE"
```

Under strict use of this hub and adapter, an identity runs on at most one member
at a time. Container creation and starting are separate engine calls, sequenced
by one live hub process. If create lands after its transport has died, that
process cannot issue the later start: the late container is **never started**.
A strict retry may already be running on another member, so inventory can contain
one `running` sandbox and a late `created` record with the same identity. The
created record holds CPU, memory and a slot in placement accounting, is visible
in inventory, and blocks further strict placement of that identity while it
exists. Use its full handle
with `release` to remove it and free that capacity. Inventory never auto-starts
created records, and a conflicting create never resumes an existing container.

Placement is **strict by default**: if any member's inventory is incomplete,
`place` rejects the request because the identity cannot be proved new pool-wide.
The optional boolean `fresh` defaults to `false`. With `fresh: true`, the caller
promises that this identity has **never been submitted before**. The hub may then
place it on a member whose complete inventory is accounted for, while other
members are unknown. **A retry must never set `fresh: true`.** An identity already
observed on any member is always rejected, even with `fresh`.

A scheduler that needs to keep working while a burst member is offline generates
a new identity and sets `fresh: true` for each new attempt. A lost response does
not justify reusing that identity as fresh. Once members return, the scheduler
reconciles possible orphaned work through `inventory` and `release`. The fresh
promise is the caller's responsibility; it is not a durable uniqueness claim the
hub can verify about an unavailable member.

Optional `preferences` is an array of numeric member IDs; the core placement
rules apply. Optional `command` is a string array; omission uses the image default.
Optional `environment` is a map of explicit strings, sent over Docker stdin. Keep
secret values out of shell history, process arguments, images and identities;
use a private producer for secret-bearing placement input.

Optional `profile` is a string naming a profile in the private member file.
Only members declaring that exact name are eligible; capacity, preferences,
strict/fresh inventory and the numeric ID tie-break rule still apply. An unknown
profile or one offered only by full or draining members has no eligible capacity.
After inventory and selection, swap below the requested memory is rejected with
`profile memory swap must be at least requested memory` before reserving capacity
or creating a container. Profile settings may differ between members offering the
same name; the selected member's settings apply.
Omitting `profile` preserves existing Docker arguments. The request accepts no
launch flags, mount paths, volumes or profile settings.

Placement returns `{"member":1,"handle":"1:<full-container-id>"}`.

For example, add `"profile":"runner"` to the placement JSON above to select
`members[].capabilities.compute.profiles.runner`. The member-file schema also
applies to `add`; atomic rewrites preserve profiles across drain, removal of
other members, and restart. Status shows names only. See [members.md](members.md)
for all typed settings and validation rules.

Anyone able to reach the owner-only socket can use an offered privileged profile.
On a rootless Docker engine that privilege stays inside the engine's user
namespace; on a rootful engine it is effectively root on the member.

The handle is the member ID and full Docker container ID, independent of inventory
order, process lifetime and the sandbox label. `release` stops and removes that
exact managed container after verifying its ownership, including when the member
is draining or its budget has shrunk. It does not remove volumes. Already absent handles return an
error; a complete `inventory` confirms absence.

Placement, inventory and removal requests refresh inventories before acting.
Release checks only its exact member; drain and add update declarations without
probing unrelated members. Every member begins unreachable. Only a complete,
bounded inventory successfully imported into the declared budget can restore its
declared admission state, after startup settling described below. Stopped and
created containers retain their allocations until explicitly released. Unknown limits, failed inspection,
unavailable engines, and rejected imports keep admission closed. Known observed
handles are retained even when their imports fail. Free budgets on a closed member
are zero; the inventory response distinguishes `inventory_complete: false` from
an accounted draining member. A failed inventory preserves prior observations as
stale handles, never as evidence of free capacity. A fresh process uses only fresh
member inventories to reconstruct its map. Numeric core reservation IDs are
deterministic hashes of the full handle with collision detection; they are never
external handles. A collision closes the affected member. A rejected import,
including two observed containers competing for a one-slot budget, leaves all
observed handles readable with the member closed; release can reconcile them.

Inventory JSON exposes each engine observation as
`members[].sandboxes[].state`, independently of the member's admission `state`.
Values are Docker state strings, including `created` (never started), `running`
and `exited`. A retained failed-start handle may have `unknown` until inspection
succeeds. When `inventory_complete` is false, retained observations may be stale;
none of those entries make capacity free. Use this field to distinguish a late
created record from the running sandbox for the same identity.

After an uncertain start or release, the hub never retries automatically. Failed
starts retain the adapter's known full container ID and sandbox identity, including
when final inspection failed. Failed inventories preserve those observations and
block the identity in this process until a successful inventory reconciles it.
Inspect `inventory` and reconcile handles before retrying. Existing containers
with duplicate labels from external or older clients remain separately accounted.

## Restart settling and request deadlines

Every new hub process closes placement and member removal for one full configured
adapter operation limit: 60 seconds by default. Drain remains available.
`inventory` remains readable during this period, with `settling: true` and zero
free budgets. Once the period ends, a subsequent placement, inventory or removal
request re-reads inventories before acting. An earlier empty listing cannot
activate or remove a member. The period also applies to fresh placements.

The delay does not prove that an engine-side create has finished. Even after
SIGTERM killed and reaped its transport and a new process's full settle period
has elapsed, a submitted create can still land. It leaves the never-started,
capacity-holding record described above; the hub does not start it later.

SIGKILL can leave an orphaned local/SSH transport. An orphaned **create** can only
leave a never-started record, because that CLI runs `container create` alone.
An orphaned **start** can act only on the exact container ID returned by an
earlier completed create, whose labeled identity is already visible in an
all-container inventory. Strict placement therefore sees that identity and
rejects another placement while it exists. The adapter uses no `docker run` or
restart policy, and the hub does not replay starts after recovery. The strict
running guarantee and the fresh warning above apply; no journal or retry
protocol is used.

The CLI supplies one absolute monotonic request deadline, 120 seconds by default,
starting before connection. It covers connection, socket queue wait, framing,
inventory, mutations and the response. The server clamps it to its own maximum;
queueing never resets it. Adapter operations receive the remaining absolute
budget and cannot start a new command after expiry. Reads, writes and connection
attempts are nonblocking and shutdown-aware. The server reserves 100 ms inside
the deadline for the response. Inventory shares half the remaining work budget
fairly among members, with every operation also capped by its adapter limit.
Several stalled members therefore return incomplete entries while leaving time
to observe a healthy member and send a usable response. A known release and drain
do not wait on those unrelated probes. Expiry or a lost response can still leave
a mutation's outcome uncertain; reconcile rather than blindly retrying.

`HOMEPOOL_OPERATION_TIMEOUT_MS` sets the adapter limit and startup settle period;
`HOMEPOOL_REQUEST_TIMEOUT_MS` sets the hub's maximum and CLI's total request wait.
Values are positive integer milliseconds; the request limit must exceed 100 ms.
Defaults live in the settings module. Keep the operation limit consistent across
restarts; if reducing it, allow the previous limit to elapse before restarting.
Tests use shorter configured limits while exercising the same process paths.

## Member lifecycle

```sh
homepool --socket "$HOMEPOOL_SOCKET" add "$MEMBER_ADDITION"
homepool --socket "$HOMEPOOL_SOCKET" drain 1
homepool --socket "$HOMEPOOL_SOCKET" remove 1
```

`add FILE` reads a private `0600` regular file containing the same version-1 schema
with exactly one member. Duplicate IDs, unknown fields and invalid endpoints fail
without writing. `drain` persists the declared draining state across restarts and
keeps existing sandboxes running. `remove` is rejected while the hub is settling,
including after a successful drain and an empty listing. Once settling ends,
removal requires a draining member with a complete empty inventory; finish or
release its observed work first. An unavailable inventory cannot justify removal.

A create submitted before removal can still land after settling and after the
member was removed. That late record is never started, but its endpoint is no
longer in the member file. Re-add the member with the same ID and Docker endpoint
(a draining declaration is suitable), then use `inventory` and `release` to
recover and remove the record. The hub retains no separate endpoint journal.

Updates rewrite the member file in canonical ID order (comments and original
formatting are not preserved), validate the entire replacement, write a `0600`
file alongside it, sync it, atomically rename it, then sync its parent directory.
A directory-sync failure reports that replacement occurred but durability could
not be confirmed. Member contents, endpoints, placement input and Docker raw
output are never written to hub logs. Inventory output contains private logical
identities and handles; keep it private.

## Local API version 1

The CLI uses newline-delimited JSON over the same Unix socket. Each connection
carries exactly one request and one response, at most 1 MiB per message.
Requests are serialized; there is no scheduler beyond placement. Every request
frame includes `deadline_monotonic_ms`, an unsigned integer expiry in milliseconds
from the local `CLOCK_MONOTONIC` clock. Raw clients compute it before connecting,
using their total wait budget; the CLI supplies it automatically. This metadata
shares the JSON object with the operation fields. Without a valid deadline the
frame is rejected. The same deadline applies to the entire connection.

The operation fields are `{"op":"inventory"}`, or
`{"op":"place",...placement fields...}` with optional `fresh`. Other operations
are `{"op":"release","handle":"..."}`, `{"op":"drain","member":1}`,
`{"op":"remove","member":1}`, and `{"op":"add","document":"..."}`;
include the deadline metadata in each frame.
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
Regressions cover both two-member outage/retry scenarios, strict rejection and
fresh placement on a healthy member, retained failed-start identities, delayed
engine completion across a genuinely new hub process, readable one-slot
budget rejection, and a queued request expiring behind multiple stalled members
without starting work. Short-deadline and post-settle SIGTERM regressions check
exactly one running sandbox and one late created record, strict rejection while
the identity is observed, occupied capacity and release recovery. The restart
removal regression drains and attempts removal before create completes, then
recovers the member and eventual handle. Real-engine adapter tests check typed
identity conflicts, created-state visibility before start, no restart policy,
and exactly one container per member and identity.

With a disposable Docker engine and `alpine:3.22` pulled, run:

```sh
cargo test -p homepool-cli --test hub -- --ignored --nocapture
```

This repeats the actual-process restart and drain test with real running
containers. `HOMEPOOL_TEST_DOCKER_SOCKET` optionally selects a local engine socket.
Tests use `TMPDIR` for private scratch and clean up test-created work. Procfs
samples sum CPU seconds across the three hub processes and report the latest
resident memory and greatest sampled process high-water mark during the test;
the PR records measured values and test conditions. These are process measurements, excluding Docker, SSH and workload resource consumption.
