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

## Owner CLI commands

The shipped `homepool` binary can stage a portion, apply the shared layout and
create a consumer directly through the adapter. These commands do not use the
hub. All three read the private member file using the usual `--config FILE`,
`HOMEPOOL_CONFIG`, then `XDG_CONFIG_HOME` selection. The member file must be a
regular file owned by the current user with mode `0600`. Each command requires
`--garage-config FILE`; the installed `garage` must be on `PATH`.

```sh
homepool --config "$HOMEPOOL_CONFIG" stage-storage 1 "$GARAGE_NODE_ID" zone-a --garage-config "$GARAGE_CONFIG"
# Review all staged changes using the existing Garage administration command:
garage -c "$GARAGE_CONFIG" layout show
homepool --config "$HOMEPOOL_CONFIG" apply-storage 1 --garage-config "$GARAGE_CONFIG"
homepool --config "$HOMEPOOL_CONFIG" create-consumer 1 --garage-config "$GARAGE_CONFIG" --credentials-file "$CREDENTIALS_FILE"
```

`stage-storage MEMBER NODE ZONE` uses that member's declared `storage_bytes`,
requires the full Garage node ID and rejects unknown members and zero storage.
`apply-storage VERSION` applies the entire shared draft at the explicitly supplied
next version. Replace `1` with the current version plus one; there is no automatic
version lookup, layout repair or retry. Review with `garage layout show` and
serialize administration with other operators before applying.
`create-consumer ID --credentials-file FILE` accepts an unsigned pool-local
numeric consumer ID. File options can appear before or after the command.
Successful mutations exit `0` with no stdout or stderr. Failures exit `1` with
fixed diagnostics that do not echo input paths or Garage output. Signal cancellation
uses the same cleanup and exit codes as the [live probe](members.md#optional-live-garage-storage).
An interrupted or timed-out mutation has an unknown outcome: inspect Garage
privately before retrying.

### Credentials file format, version 1

The destination must not exist, including a directory or dangling symlink. Its
parent must pass the existing private-path checks: a directory owned by the
current user with mode `0700`, without a symlink at that directory entry. Missing
parents are created privately. The CLI reserves a new regular file with mode
`0600` **before any Garage call**, then writes the complete UTF-8 JSON document,
adds a newline and syncs the file before reporting success. An existing file is
never overwritten. Keep the directory private and do not replace its entries
while provisioning.

The stable version-1 JSON fields are:

| Field | Value |
| --- | --- |
| `version` | Integer `1`. |
| `bucket` | String `homepool-consumer-ID`. |
| `aws_access_key_id` | Newly issued access key ID string. |
| `aws_secret_access_key` | Newly issued secret access key string. |

Read the file directly in your S3 client, for example with Python and boto3:

```python
import json
import os
import boto3

with open(os.environ["CREDENTIALS_FILE"], encoding="utf-8") as source:
    credentials = json.load(source)
if credentials["version"] != 1:
    raise ValueError("unsupported credentials file version")
s3 = boto3.client(
    "s3",
    endpoint_url=os.environ["S3_ENDPOINT"],
    region_name=os.environ["S3_REGION"],
    aws_access_key_id=credentials["aws_access_key_id"],
    aws_secret_access_key=credentials["aws_secret_access_key"],
)
s3.put_object(Bucket=credentials["bucket"], Key="sample", Body=b"shared object")
```

The endpoint and region come from the separately operated Garage installation.
Never print the document or pass keys through command arguments or logs. If Garage
issues credentials but the write or sync fails, the CLI removes the incomplete
file where possible and reports: `Garage issued credentials but the credentials
file could not be written; the consumer may exist; inspect Garage privately before
retrying`. The consumer is not automatically deleted or retried; inspect and
revoke it through Garage administration before creating a replacement. Other
provisioning failures also remove the reserved file where possible and preserve
the adapter's nontransactional failure semantics below.

An ungraceful termination such as SIGKILL or a crash cannot run this cleanup.
An empty, partial or complete-but-unconfirmed credentials file can remain at the
destination and will block another attempt. **File existence alone is not
success.** Inspect and validate any surviving document and the corresponding
Garage state privately. Keep usable credentials; do not discard them merely
because success was not reported. If they are unusable, revoke any orphaned keys
and clean up orphaned consumer resources through Garage administration, then
remove the leftover file and retry. Do not remove it and retry before reconciling
Garage's state.

**A single-node layout keeps a single copy. It is not a backup.**

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

Use `with_cancellation(Arc<AtomicBool>)` to share a cooperative cancellation flag
with the adapter. Setting it to true stops the process group and reaps its child
before returning the fixed `Error::CANCELLED`. The flag remains set and prevents
subsequent operations from starting. Cancellation has the same unknown-outcome,
inspect-before-retry rule as a deadline and never automatically retries or launches
cleanup requests after cancellation. During a probe or storage command the CLI sets this flag on
SIGINT and SIGTERM; see [status exit codes](members.md#optional-live-garage-storage). Library callers
own signal handling and must keep the supervisor alive until cancellation returns.

## Status and verification

Add `--garage-config FILE` to `homepool status` to probe live logical usage through
that existing installation. See [the status contract](members.md#optional-live-garage-storage).
No member file changes are required. Without the option, status performs no live
probes. Failed probes fail the command without partial output.

The CI `test` job runs the opt-in integration test on a GitHub-hosted runner:

```sh
cargo test -p homepool-adapter-garage --test garage --locked -- --ignored
cargo test -p homepool-cli --test garage --locked -- --ignored
```

Set `TMPDIR` to a private scratch directory and provide Docker locally. Both tests
use version-and-digest-pinned Garage and Python images. It creates an isolated
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

The CLI integration runs `stage-storage`, `apply-storage` and `create-consumer`
against a real single-node Garage, then reads the credentials file and performs
S3 writes and reads in fresh client containers. It also checks the file mode,
silent successful output, existing-file refusal, unknown nodes and wrong versions.
Local CLI regressions exercise zero-storage and unknown members, private path and
argument diagnostics, and a failed credentials write after issuance.
