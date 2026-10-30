# Docker adapter

`homepool-adapter-docker` uses the installed Docker CLI with an explicit `--host`
for every operation. This reuses Docker's API negotiation and SSH transport without
adding an HTTP, TLS, or SSH client dependency. The only external Rust dependencies
are the workspace's existing `serde` and `serde_json` for structured responses.
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
limits must be positive. Bind mount sources refer to the **engine's** filesystem,
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
only ids, labels, and state, never environment values.

Docker's [create reference](https://docs.docker.com/reference/cli/docker/container/create/)
and [env-file parser](https://github.com/docker/cli/blob/master/pkg/kvfile/kvfile.go)
describe the CLI behavior used here. The
[SSH guide](https://docs.docker.com/engine/security/protect-access/) describes
Docker's SSH transport.

## Local integration tests

The ordinary workspace suite runs unit tests without requiring an engine.
The CI `test` job additionally pulls `alpine:3.22` and runs the ignored engine
tests explicitly. Missing or inaccessible Docker fails that step.

With a disposable local engine and a writable `TMPDIR`:

```sh
docker pull alpine:3.22
cargo test -p homepool-adapter-docker --test engine -- --ignored --test-threads=1
```

Set `HOMEPOOL_TEST_DOCKER_SOCKET` to test a custom local socket. Tests create
containers and bind-mount scratch files under `TMPDIR`; cleanup runs on normal
completion and assertion failures. Tests cover capacity, lifecycle, labels,
limits, mounts, environment input beyond pipe-buffer capacity, ownership checks,
and rollback after a failed start. They never contact an SSH host.

## Manual SSH verification

Use a disposable member and an existing SSH configuration. The address below is
from the documentation range; replace it with a member you own. Do not commit
real connection details. Configure key or agent authentication, verify the host
key interactively first, and ensure the remote login can run Docker. For automation,
set `BatchMode yes` and a bounded `ConnectTimeout` in that member's SSH stanza.
The adapter is synchronous; operation deadlines follow Docker and SSH settings.

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
