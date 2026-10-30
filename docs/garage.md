# Garage storage adapter

`homepool-adapter-garage` drives an **existing, separately installed Garage 2.x**
through `garage json-api`. It does not download, bundle, link or vendor Garage.
The adapter has been tested against Garage 2.1.0. See Garage's
[administration API documentation](https://garagehq.deuxfleurs.fr/documentation/reference-manual/admin-api/)
for the JSON-over-stdin interface. The adapter requires a Unix client. It uses
`serde` and `serde_json` for JSON and `rustix` for safe nonblocking pipes and
process-group control, in addition to core. It never depends on another adapter.

Install and operate Garage separately. Give its private configuration file the
RPC connection settings and restrict access to that file and its executable.
`Garage::new(config)` uses the installed `garage` on `PATH`.
`Garage::with_command(executable, prefix_arguments)` supports a deployment wrapper,
such as a container exec command. These are trusted program inputs, not a shell
command string. Never put credentials in prefix arguments.

## Member portions

Use `Garage::stage_portion(&member, node_id, zone)` with the full Garage node ID
and a failure-domain zone. This stages exactly the member's `storage_bytes`
contribution as the node's capacity, with a `homepool-member-ID` tag. It replaces
that node's role and tags, preserving other nodes' roles. Zero contributions,
invalid node IDs and empty zones fail before invoking Garage. A full ID must also
resolve to a node in `GetClusterStatus` before any role is staged. Unknown nodes or
failed cluster queries leave the draft untouched; joining nodes remains separate
administration. Zero does not mean
remove a node; removal and draining remain explicit Garage administration tasks.

Review the shared draft with `garage layout show`, then call
`Garage::apply_layout(next_version)` with the current layout version plus one.
Staging alone does not change the running layout. Applying activates **all**
staged changes, so serialize administration with other operators. Garage can reject
an invalid layout or an incorrect version. The adapter does not change replication
settings, join nodes, or automatically retry layout mutations.

Layout capacity guides partition placement; it does not enforce a disk quota.
Reserve the underlying storage separately and configure Garage bucket quotas if
needed. Logical object usage is distinct from physical space, replication overhead,
and free filesystem capacity.

**A single-node layout keeps a single copy. It is not a backup.** A lost disk or
member can lose the store. Choose replication and independent backups appropriate
to the data before trusting it to the pool.

## Consumers and credentials

`Garage::create_consumer(consumer_id)` creates the globally unique bucket alias
`homepool-consumer-ID` and one fresh access key with a matching descriptive name.
Use an opaque unsigned pool-local numeric ID. The key can read and write that
bucket; it has neither bucket-owner rights nor permission to create buckets.
It receives no rights to other consumers' buckets. These aliases are reserved for
homepool; external administration can change permissions and must preserve this
isolation.

An existing bucket is rejected before another key is issued. Provisioning is not
transactional: after a failure the adapter attempts to delete known newly created
resources, but interrupted RPCs or failed cleanup require private inspection in
Garage before retrying. It never takes over an existing bucket, recovers credentials
by name, or rotates credentials implicitly.

On success, deliver `consumer.credentials.access_key_id()` and
`consumer.credentials.secret_access_key()` directly to the consumer's private
credential store. The adapter does not persist them. Dropping the returned value
does not revoke the key; use Garage's administration tools for revocation.
Requests travel through stdin, responses are captured in memory, subprocess stderr
is discarded, and adapter errors contain only fixed diagnostics. Credentials have
redacted Debug output and no serialization implementation. Never print them or put
them in command-line arguments, logs, status, or errors. Normal memory is used;
there is no promise to zeroize credentials. The calling application and any wrapper
must maintain the same confidentiality rules.

## Operation deadlines

Each public adapter operation has a 60-second default deadline, defined in the
adapter's settings module. Configure it with
`Garage::new(config).with_operation_timeout(Duration::from_secs(30))?` (also
available after `with_command`). Zero and unrepresentable durations are rejected.
The same budget covers all subprocess calls, stdin, output, process completion,
cluster resolution and attempted provisioning cleanup. It does not restart for
each request or bucket. On expiry the adapter kills the local process group,
reaps its direct child, closes the nonblocking pipes, and returns a fixed sanitized
deadline diagnostic. There are no background pipe threads left waiting on a wrapper.

Killing the client cannot cancel a mutation already received by Garage or a remote
program behind a deployment wrapper. A timed-out mutation has an **unknown outcome**:
inspect the installation privately before retrying. No operation is retried
automatically, and cleanup cannot launch another request after the budget expires.

## Status and verification

Add `--garage-config FILE` to `homepool status` to probe live logical usage through
that existing installation. See [the status contract](members.md#optional-live-garage-storage).
No member file changes are required. Without the option, status performs no live
probes. Failed probes fail the command without partial output.

The CI `test` job runs the opt-in integration test on a GitHub-hosted runner:

```sh
cargo test -p homepool-adapter-garage --test garage --locked -- --ignored
```

Set `TMPDIR` to a private scratch directory and provide Docker locally. The test
uses version-and-digest-pinned Garage and Python images. It creates an isolated
single-node layout without publishing ports, assigns a member portion, provisions
two consumers, writes in one client container and reads in another through S3.
It verifies both directions of read isolation, denied cross-bucket writes and
bucket creation, exact key permissions, no duplicate consumer key, and converging
usage counters. It also rejects an unknown full node ID without changing the layout
or draft and confirms repeated S3 writes still succeed. Transport regressions cover
blocked stdin, stalled responses, inherited output pipes, process completion,
concurrent pipe traffic, child reaping and a shared multi-request deadline.
The Python client uses only its standard library. Generated test
credentials travel only through stdin and remain inside the test; containers and
scratch files are removed on normal completion or a test assertion failure.
