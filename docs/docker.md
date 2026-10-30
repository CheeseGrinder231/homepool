# Docker adapter

`homepool-adapter-docker` uses the installed Docker CLI with an explicit `--host`
for every operation. This reuses Docker's API negotiation and SSH transport without
adding an HTTP, TLS, or SSH client dependency. Structured responses use `serde`
and `serde_json`; safe Unix process and pipe control uses `rustix`.
It requires a Unix client with `/dev/stdin` and a Linux Docker engine. It creates
ordinary containers, with the isolation guarantees of that engine.

Construct `Endpoint::unix` with the local socket path, including a custom rootless
socket. For SSH, construct `Endpoint::ssh(host, port, remote_socket)`. A missing
remote socket selects Docker's remote default. SSH authentication and login names
come from the caller's SSH configuration or agent. Endpoint construction rejects
embedded credentials and command options. The adapter removes inherited Docker
context and TLS endpoint overrides; it does not change Docker or SSH configuration.
Remote socket paths are restricted to ASCII letters, digits, slash, dash, dot, and
underscore to avoid remote-shell interpretation by older Docker clients.

Create `Docker::new(endpoint, MemberId(...))` to scope operations to one member.
`start` creates and starts a `SandboxSpec` with CPU in millicores, memory in bytes,
bind mounts, environment, and an optional nonsecret command override. Both resource
limits must be positive. CPU requests must be multiples of 10 millicores;
`start` rejects other precision with `Error::CpuLimitNotAligned` before creating
anything. Callers reserve that same aligned CPU amount. Bind mount sources refer
to the **engine's** filesystem,
including over SSH. Paths containing CSV separators or quotes are rejected.
The sandbox id is a descriptive label, not a uniqueness or retry guarantee.
Use the returned full container id for `stop` and `remove`.

`list` includes stopped containers and filters all three labels: `homepool.managed`,
`homepool.member`, and `homepool.sandbox`. Stop and remove independently verify
ownership before acting. Container ids are validated as full hexadecimal ids.
Labels identify management scope; an engine administrator can forge them, so they
are not an authorization boundary. Removing a running container fails: stop it
first. Volumes are never deleted. If starting fails after creation, the adapter
attempts cleanup; a cleanup failure returns the container id for manual recovery.
Communication failures can leave an unknown result; reconcile with `list` before
retrying creation. List errors, including concurrent removal, are returned rather
than converted into empty results. Capacity reads report engine CPU and memory
totals; free capacity, slot counts, and placement remain caller policy.

## Deadlines and reconciliation

Every public operation has a 60-second default deadline. Configure it with
`Docker::new(endpoint, member).with_operation_timeout(Duration::from_secs(30))?`.
The deadline covers the entire call: list and all inspections, ownership checks
and mutations, or create/start/inspection and attempted rollback. A slow command
does not reset the clock. Zero and unrepresentable durations are rejected.

The adapter uses a dedicated process group for the CLI and its SSH transport.
On expiry it kills the group, reaps the CLI, and closes the nonblocking pipes;
stdin cannot leave a blocked writer behind. Cleanup after a failed start shares
the same deadline. `StartFailed` retains the known container id, redacted
`start_error`, optional `cleanup_error`, and `outcome_uncertain`. A `Deadline`
cause identifies which phase expired. Successful rollback has no cleanup error
and confirms removal; any failed rollback leaves `outcome_uncertain: true`.
If start exhausts the deadline, cleanup also reports a deadline without spawning
a new command. A create timeout may have no known container id.

Terminating the client does not cancel a mutation already received by an engine.
After a deadline, communication error, failed cleanup, or failed final inspection,
keep the member closed to new placement and reconcile with a successful `list`.
Match known work by `(member id, full container id)` and inspect its current state.
For an uncertain create, examine all matching sandbox labels for duplicate or
unclaimed containers before deciding what to retain or remove. The sandbox label
is descriptive and has no uniqueness or retry guarantee. Reconcile before retrying
an uncertain start; blind retries can create a second sandbox. Do not release a
reservation just because stop/remove timed out. Confirm the resulting state first.
An incomplete or failed list is never evidence that a sandbox is absent.

## Restart accounting

`Sandbox` includes `cpu_millicores: Option<u64>` and `memory_bytes: Option<u64>`.
CPU is a conservative bound on the tighter configured NanoCPUs or CFS
quota/period ceiling, rounded up to the next 10 millicores (already aligned values
stay unchanged). The runtime's systemd quota translation rounds fractional CPU
percentages upward: one percent of a CPU is 10 millicores. Merely rounding to a
whole millicore could report 501 when the applied cgroup permits 510. The named
`CPU_LIMIT_QUANTUM_MILLICORES` constant in adapter settings covers that normalization
for both NanoCPUs and quota-only inventory. Requiring aligned starts keeps the CPU
amount callers reserve equal to what is applied. Docker's zero/default CPU period
resolves to the 100,000-microsecond CFS default, so a quota-only value of 50,000 means
500 millicores. The protocol default is documented in the adapter settings module.
Memory is the configured hard byte limit.
A missing finite ceiling is `None`, never a zero-sized allocation. CPU affinity,
parent cgroups, and engine totals can further restrict execution; these observations
do not infer free host resources. Malformed or missing limit fields fail the list.
Unbounded observations require caller policy; keep admission closed rather than
importing them as zero usage.

To rebuild a pool, add members as draining or unreachable, then inventory each
member fully. Retain or deterministically reconstruct a collision-checked mapping
from `(member id, full container id)` to `ReservationId`; do not use the sandbox
label as a unique key. Pass each observed allocation and its stable id to
`Pool::import_allocation(Reservation { id, member_id, budget })`, using the observed
CPU, memory, and one slot. Import preserves the exact member, even when draining
or unreachable, rejects duplicate outstanding ids and over-budget work, and
advances subsequent generated ids beyond all imported ids. Import order does not
matter. The caller must not repurpose an id for a different allocation.

Only mark a member active after its complete inventory has been accounted for.
A partial read, unsupported limits, or failed import must leave it unschedulable.
Draining members stay draining after restoration. `list` also includes stopped and
created containers: the caller must explicitly reconcile their lifecycle before
releasing allocations or removing them. Neither import nor a state change implies
that work has ended. Core performs no persistence or engine I/O; the caller owns
the stable mapping and admission transition.

## Secrets

Put all secret values in `Environment`, never in images, commands, paths, ids, or
labels. All environment entries, secret or otherwise, are streamed to the Docker
CLI through stdin using `--env-file /dev/stdin`. No temporary secret files are
created. Keys must be portable environment-variable identifiers, values cannot
contain newline, CR, or NUL, and each encoded line must be under Docker's 64 KiB
scanner limit. Entries always contain `=`, so Docker cannot inherit a value from
the caller's environment. Whitespace, quotes, and `=` within values are preserved.

These values become container environment variables. Docker administrators and
processes with inspection access can read them; this is a transport rule, not an
encrypted secret store. `Environment` debug output redacts values. Adapter errors
omit raw Docker output, which could echo sensitive input. Inspect operations read
only ids, labels, state, and resource limits, never environment values.

Docker's [create reference](https://docs.docker.com/reference/cli/docker/container/create/)
and [env-file parser](https://github.com/docker/cli/blob/master/pkg/kvfile/kvfile.go)
describe the CLI behavior used here. The
[SSH guide](https://docs.docker.com/engine/security/protect-access/) describes
Docker's SSH transport.

## Local integration tests

The ordinary workspace suite runs unit tests without requiring an engine.
The CI `test` job additionally runs the ignored Docker CLI regression, pulls
`alpine:3.22`, and runs the ignored engine tests explicitly. Missing or inaccessible
Docker fails that step.

With a disposable local engine and a writable `TMPDIR`:

```sh
cargo test -p homepool-adapter-docker --lib -- --ignored
docker pull alpine:3.22
cargo test -p homepool-adapter-docker --test engine -- --ignored --test-threads=1
```

Set `HOMEPOOL_TEST_DOCKER_SOCKET` to test a custom local socket. Tests create
containers and bind-mount scratch files under `TMPDIR`; cleanup runs on normal
completion and assertion failures. Tests cover capacity, lifecycle, labels,
limits, mounts, environment input beyond pipe-buffer capacity, ownership checks,
rollback after a failed start, and restart accounting for different sandbox sizes.
A Linux fake-engine test accepts a local connection without responding, then
expires the private test clock after readiness. The same seam covers blocked stdin,
inherited pipes, start expiry, and rollback expiry. Tests assert transport
termination, direct-child reaping, and pipe closure; an outer watchdog detects hangs
without making startup speed a correctness condition. Unit tests also cover large
simultaneous pipe traffic. Engine tests cover quota-only inventory and lifecycle,
and compare non-aligned quota-only and NanoCPU observations with applied cgroup
CPU limits; inventory must never report less than the cgroup permits. If controller
files are not exposed, the case is explicitly logged as unavailable so the PR
validation report can identify that limitation. Tests never contact an SSH host.

## Manual SSH verification

Use a disposable member and an existing SSH configuration. The address below is
from the documentation range; replace it with a member you own. Do not commit
real connection details. Configure key or agent authentication, verify the host
key interactively first, and ensure the remote login can run Docker. For automation,
set `BatchMode yes` and a bounded `ConnectTimeout` in that member's SSH stanza.
The adapter deadline also bounds local SSH transport; SSH connection settings may
fail earlier.

1. Run `docker --host ssh://192.0.2.1 info`. If the member uses a custom port or
   remote rootless socket, append it, for example
   `ssh://192.0.2.1:2222/run/member/docker.sock`.
2. In the calling application, construct
   `Endpoint::ssh("192.0.2.1", None, None)` and a `Docker` for a test `MemberId`.
   Use `Some(2222)` and `Some(Path::new("/run/member/docker.sock"))` for the
   custom endpoint. Confirm `capacity()` matches the engine's CPU and memory.
3. Start a small `alpine:3.22` sandbox with `sleep 60`, positive CPU and memory
   limits, and a dummy registration token in `Environment`. Confirm `list()`
   returns its id with the intended sandbox label.
4. Inspect its limits and labels using the CLI. Confirm the dummy token is absent
   from process arguments and labels. Container inspection can read its environment.
5. Call `stop()` then `remove()` with the returned id and confirm `list()` no longer
   contains it. Repeat with a bind mount whose source exists on the remote member.
