# Local HTTP API

`homepool api` runs as a separate process and connects to the hub as an ordinary
client over its existing owner-only Unix socket. The hub still opens no network
listener. Start the hub separately; the API can start and stay up while it is down.

**This slice has no TLS. It is loopback-only for that reason.** `--listen` requires
a numeric IPv4 or IPv6 loopback socket address; wildcard and non-loopback addresses
are refused before binding. There is no browser or CORS support.

```sh
homepool --socket "$HOMEPOOL_SOCKET" api --listen 127.0.0.1:8080 --token-file "$HOMEPOOL_API_TOKEN_FILE"
```

The token file must be a regular file owned by the current user with mode `0600`,
in an immediate parent directory owned by that user with mode `0700`. Symlinks,
missing files, empty files and invalid token contents stop startup. Provision a
random, unguessable token privately, outside the repository. Tokens contain 1–256
ASCII letters, digits or `-._~+/`, optionally followed by `=` padding. One final
LF or CRLF is allowed in the file. Whitespace inside the token is invalid.

The token is read once at startup; restart the API to rotate it. Send it as
`Authorization: Bearer TOKEN`. Exactly one authorization header is accepted.
Token bytes and length are compared in constant time after parsing into fixed
size buffers. The API never logs tokens, requests or raw hub errors.

Socket selection is the same as the CLI: `--socket FILE`, `HOMEPOOL_SOCKET`, then
`$XDG_RUNTIME_DIR/homepool/hub.sock`. The API needs no member configuration file.

## Endpoints

- `GET /v1/health` needs no authentication and returns only `{"version":1}`.
  This reports the API version, independently of hub availability.
- `GET /v1/sandboxes` requires authentication. Wrong, missing, malformed and
  duplicate credentials return `401` before any hub connection. A successful
  response has this shape:

```json
{"inventory_complete":true,"sandboxes":[{"member":1,"sandbox_id":"example-job","state":"running","exit_code":null}]}
```

`exit_code` is an integer for exited sandboxes and `null` when unavailable.
`inventory_complete` is false if any member has incomplete inventory or the hub
is still settling. Entries may then be stale. Only the four listed sandbox fields
are exposed; handles, resource budgets and member configuration are omitted.
Unknown fields in the hub inventory schema fail closed with `503` until reviewed.
Hub connection, protocol and inventory response failures return `503` with an
empty body. The API stays available and reconnects on the next authorized request.

Unknown paths return `404`; unsupported methods on known paths return `405`.
Neither contacts the hub. Authenticated GET and DELETE endpoints reject bodies
and accept only absent framing or a single `Content-Length: 0`. Every response closes its
connection; there is no keep-alive, streaming or pipelining. The API provides no
stored results or cancellation endpoint.

## Approved operations and jobs

Supply `--operations-file FILE` to enable approved work. Without it, the catalog
is empty. This private TOML file has the same regular-file, ownership, `0600`
and immediate-parent `0700` requirements as the token file. It is bounded to
1 MiB and read once at startup; restart to apply changes. Invalid files fail
with a fixed error that never includes their contents. Keep it outside the
repository. For example:

```toml
version = 1
[[operations]]
name = "check"
image = "alpine:3.22"
command = ["sh", "-c", "exit 7"]
cpu_millicores = 100
memory_bytes = 33554432
[operations.environment]
EXAMPLE_OPTION = "approved-value"
```

Operation names are unique and contain 1–59 ASCII letters, digits or `-_.`.
Image, command, positive CPU and memory limits are required; CPU is a multiple
of 10 millicores. Environment is optional and uses the existing Docker stdin
path. No process environment is inherited. Unknown fields, including `profile`,
are rejected: operations cannot select privileged profiles in this slice.
Environment values are omitted from catalog and job metadata responses and
diagnostics. A job can still print those values into its logs.

All operation and job endpoints require authentication before any hub connection:

- `GET /v1/operations` returns a sorted JSON array of operation names only,
  such as `["check"]`. It does not contact the hub.
- `POST /v1/jobs` accepts exactly `{"request_id":"example-job","operation":"check"}`.
  The body must have one `Content-Length`, no transfer encoding, and be at most
  4 KiB. Malformed JSON, duplicate or unknown fields return `400`; oversized
  bodies return `413`. Clients cannot supply images, commands, environment,
  profiles, flags or any other launch settings. Unknown operations return `404`.
- `GET /v1/jobs/example-job` reads state and exit code from hub inventory.
  A complete inventory without that identity returns `404` with state `absent`.
- `DELETE /v1/jobs/example-job` releases only an exited job. Success returns
  `200` with the observed operation and exit code, and state `released`:

  ```json
  {"request_id":"example-job","operation":"check","state":"released","exit_code":0}
  ```

  Any other observed state returns `409` with that state and leaves the job alone.
  Complete inventory without the identity returns `404` with state `absent`,
  including a retry after a lost successful release response. Incomplete inventory
  or hub settling returns `503` with state `unknown` and releases nothing.
  Release discards the job's output and frees its capacity. The request ID can
  then be reused, including for another operation. Jobs whose operations have
  left the catalog can still be released. An uncertain release returns `503`
  with state `unknown`; it is never automatically replayed or redirected to a
  replacement job. Status can reconcile the outcome.
- `GET /v1/jobs/example-job/logs` reads output for the same API-derived identity
  using the hub's existing `logs` request at its maximum tail of 1,000 lines per
  stream. It accepts no query string. A successful response looks like:

```json
{"request_id":"example-job","operation":"check","state":"exited","stdout":"...","stderr":"","cut":false}
```

The logs endpoint returns `404` with state `absent` only with complete inventory.
Incomplete inventory, including hub settling, returns `503` with state `unknown`
and no output; it never claims absence or reads logs from a stale handle.
Hub connection, schema and log-read failures return `503` with an empty body.
Only API-derived identities can be read; an ordinary sandbox placed directly
through the hub cannot be addressed by supplying its sandbox ID or handle.
A hub socket client has full owner authority. A sandbox deliberately placed
through that socket under a valid derived identity is treated as an API job,
including when the operation is no longer in the current catalog. The identity
is an opt-in namespace, not proof that the API created the sandbox.

Output is retained by Docker and can be read while a job runs and after API and
hub restart, until release. `cut` is true if the line or byte limits discarded
output. The combined raw byte cap is 174,592 bytes, split equally between stdout
and stderr; truncation keeps suffixes and invalid UTF-8 becomes replacement
characters. There is no following, storage or release through this endpoint.

**The response contains whatever the job printed. An approved operation that
prints a secret exposes it to anyone with the API token. Operations must not
print secrets.** Output is never included in API diagnostics.

A successful submission or status response looks like:

```json
{"request_id":"example-job","operation":"check","state":"exited","exit_code":7}
```

Request IDs follow the existing sandbox identity grammar: 1–128 ASCII letters,
digits or `-_.`. Invalid IDs return `400`. A sandbox identity is
`api-<SHA-256 of request_id in lowercase hex>-<operation name>`; this fits the
existing 128-byte identity limit and lets a new API process recover the operation
from Docker without reading environment values. The complete suffix must follow
the operation-name grammar above, independently of catalog membership. An empty
or invalid suffix is ignored by status, logs, release and submission lookup: with
complete inventory, reads return `404` with state `absent`, and an approved submission can
place a distinct valid job rather than conflict with or reuse that sandbox.

The same request ID and operation returns the observed job without placing it
again, including after API or hub restart. A different operation for an existing
ID returns `409`, even when that operation is no longer in the current catalog.
Submissions are serialized. An API with an operations file holds an exclusive
owner-only sidecar lock beside the hub socket, so another submitting API using
that socket path refuses startup. Use one consistent socket path per hub. The
lock contains no job data and requires no manual removal after process exit.

Placement is strict, without `fresh`: incomplete inventory closes admission.
When any member's inventory is incomplete or the hub is settling, status is
`unknown` with a null exit code, even if a retained observation says running or
exited. An unplaceable job (including a full pool), an incomplete inventory or an
unresolved placement returns `503`; when placement was attempted its response
contains state `unknown`. A lost placement response is reconciled by inventory,
never replayed automatically. `200` reports an observed state such as `created`,
`running` or `exited`; `created` does not mean the job started. Connection or
schema failures can return `503` with an empty body. Retry or read status to
reconcile uncertain outcomes. No error exposes raw hub diagnostics.

**A job's identity lasts only until it is released.** This slice stores no
receipts. A released job's request ID can be reused, including for a different
operation. Exited jobs retain capacity until released through the API or the
owner's hub/CLI.

## Limits

The request line and headers together are limited to 8 KiB and 32 header fields;
oversized requests receive `431`. Invalid HTTP receives `400`. An absolute
one-second header deadline and a five-second header-and-body deadline, both measured
from acceptance, prevent slow trickles from extending the wait;
timeouts receive `408` when the peer can still receive a response. Response writes
have at most five seconds. Up to 32 connections run concurrently; excess
connections are closed without queuing. A stalled client does not serialize other
clients. The hub still serializes its own requests.

A logs response body is capped at 1,048,576 bytes (1 MiB), including JSON escaping
and job metadata. With HTTP headers, the complete response is at most 1,048,696
bytes. Reading this within the five-second write allowance requires about
1.68 Mbit/s of payload throughput, within a normal local or forwarded link's
capacity. Exhausting the write budget can close a `200 OK` response partway
through its body, leaving incomplete JSON. Clients must compare the received
body length with `Content-Length`, reject a short read and retry the request.
Oversized hub output fails closed with `503`; the API never enlarges the hub's
existing log cap.

After writing a response (including `401`), the API shuts down its write side to
deliver the response before closing with unread input. It drains at most 12 KiB
of input already available, without waiting for more bytes or peer EOF, then
releases the slot. A peer may observe a reset after the response if input remains.

The shorter header deadline applies equally to clients sharing an address and
clients with different addresses; there is no per-address quota. It bounds slot
occupancy, not availability: unauthenticated peers can still fill all 32 slots
with incomplete headers for up to one second (plus response-write time and
scheduling), and keep doing so by opening replacement connections. New connections
are then closed even if they would supply a valid token, because admission happens
before headers and authentication are read. An unauthenticated peer that does not
read its response can also occupy a slot for the five-second write allowance if
the write blocks. This is not protection against sustained connection exhaustion.

All hub calls for one HTTP request share the existing bounded client path and
one `HOMEPOOL_REQUEST_TIMEOUT_MS` budget (120 seconds by default). The complete HTTP
connection is bounded by that budget plus the two five-second I/O allowances.
All defaults and caps live in the CLI settings module.

## Tests

`cargo test --workspace` covers authentication without a hub call using a Unix
socket trap, startup refusal, private token and operations files, schema
projection, malformed and oversized bodies, forbidden launch fields, stalled
clients, nonblocking slot release after `401`, early errors with unread bodies,
full pools, unreachable members, concurrent submissions, restart recovery
and a lost hub placement response followed by a retry. The normal CI Docker integration
step also runs `api_engine_lists_and_authenticates`: the shipped hub places a real
sandbox, the shipped API lists it, and rejected credentials never contact the hub.
The same CI step runs `api_engine_submits_retries_and_observes_exit_code`: the
shipped API submits approved work through a real hub to real Docker, retries
without a second sandbox, reads real stdout and stderr, restarts both processes,
and reads the same output and exit code 7. It releases the exited job through
the API and runs a new request ID in the freed slot on a one-slot member. Tests
also cover release conflicts without stopping running work, release races and
lost responses, ordinary sandbox isolation, and incomplete inventory,
cut output, worst-case JSON escaping and refusal to read owner-placed sandboxes.
To repeat both with a disposable Docker engine and `alpine:3.22` already pulled:

```sh
cargo test -p homepool-cli --test hub api_engine_ -- --ignored --nocapture
```
