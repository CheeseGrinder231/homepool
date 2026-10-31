# Read-only local HTTP API

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

Unknown paths return `404`; methods other than GET on known paths return `405`.
Neither contacts the hub. The API provides no placement, release or log operations.
GET requests with body framing other than `Content-Length: 0` are rejected on the
sandbox endpoint. Every response closes its connection; there is no keep-alive,
request-body processing, streaming or pipelining.

## Limits

The request line and headers together are limited to 8 KiB and 32 header fields;
oversized requests receive `431`. Invalid HTTP receives `400`. An absolute
five-second header deadline prevents slow trickles from extending the wait;
timeouts receive `408` when the peer can still receive a response. Response writes
have at most five seconds. Up to 32 connections run concurrently; excess
connections are closed without queuing. A stalled client does not serialize other
clients. The hub still serializes its own requests.

Each hub call uses the existing bounded client path and its
`HOMEPOOL_REQUEST_TIMEOUT_MS` budget (120 seconds by default). The complete HTTP
connection is bounded by that budget plus the two five-second I/O allowances.
All defaults and caps live in the CLI settings module.

## Tests

`cargo test --workspace` covers authentication without a hub call using a Unix
socket trap, startup refusal, private token files, schema projection, oversized
headers, stalled clients, hub outage and recovery. The normal CI Docker integration
step also runs `api_engine_lists_and_authenticates`: the shipped hub places a real
sandbox, the shipped API lists it, and rejected credentials never contact the hub.
To repeat it with a disposable Docker engine and `alpine:3.22` already pulled:

```sh
cargo test -p homepool-cli --test hub api_engine_lists_and_authenticates -- --ignored --nocapture
```
