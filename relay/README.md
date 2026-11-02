# Relay Worker (slice 2 of #50)

The Worker joins one bridge and up to four phones per configured route. No npm
runtime dependencies, bridge process, page UI, deployment, or account setup is
included. The wire contract is in [off-lan-protocol.md](../docs/off-lan-protocol.md).

`wrangler.toml` is deliberately disabled: empty route allowlist, kill switch on,
no domain/account/route/secret values. An operator supplies private configuration
separately. `BRIDGE_TOKENS` is a Worker secret containing a JSON map from allowed
route IDs to independent 32-byte tokens encoded as 64 lowercase hex digits.
Only the route is in the URL path; queries are rejected. Tokens are binary first
messages. The code emits no logs and observability is disabled. The platform
still sees request URLs/IPs and connection metadata; this is not anonymity.

Unknown routes are rejected before either namespace is accessed. A separate
singleton Durable Object serializes the global upgrade rate; route objects
serialize admission, storage budgets, token changes and forwarding. A second
bridge claimant never replaces the admitted socket. Bridge loss closes phones;
reconnect starts fresh sessions with IDs starting at one. Session zero controls
are privileged to the admitted bridge. The relay interprets no Noise content.
The bridge remains responsible for device/invitation authorization and storage.

## Bounds and recovery

Defaults live in `limits.mjs`. Rates use UTC epoch-aligned one-second windows;
per-route budgets use UTC days. Socket attachment counters survive hibernation;
budgets and global upgrade counters are committed to SQLite-backed DO storage
before forwarding or granting an upgrade. Storage failures fail closed.

| Limit | Value | On exhaustion / recovery |
| --- | --- | --- |
| Phones per route | 4 | Refuse admission; retry after a slot is fully drained |
| Pending admissions per route | 8 | HTTP 429; retry after admission/timeout releases a slot |
| First-message deadline | 5 seconds | Close; establish a fresh connection |
| Binary envelope | 65,541 bytes | Close offending socket; reconnect with valid sizes |
| Incoming socket traffic | 256 messages and 8 MiB per second | Close offender; reconnect and obey the next window |
| Incoming controls per socket | 32 messages and 4 KiB per second, also subject to total rate | Close offender; reconnect with lower rate |
| Outstanding data per session per direction | 1 MiB | Error and close that session before the next send; fresh session required |
| Aggregate bridge outstanding data | 4 x 1 MiB | Derived from the session cap; each session isolated |
| Application credit window | At most 512 KiB including framing | Endpoint obligation; tested with 492,032-byte bursts |
| Daily incoming bytes per route | 1 GiB | Close offender / reject upgrades; UTC day reset |
| Daily events per route | 1,000,000 | Close offender / reject upgrades; UTC day reset |
| Daily failed upgrades/admissions per route | 1,000 | Refuse further upgrades; UTC day reset |
| Global upgrade grants | 20 per second | HTTP 429; next second |
| Registered hashes per route | 128 | Close bridge on overflow; reconnect and remove unused bindings |
| Operator kill switch | `KILL_SWITCH` other than literal `false` | Reject upgrades and close sockets on their next event; normal bridge keepalive checks within 15 seconds |

Daily events include upgrade attempts reaching the route and incoming socket
messages, including controls and failed admission bookkeeping. Byte budgets count
incoming envelopes once, including controls, before forwarding; there is no
fanout of data. Failed admissions, deadline expiry and pending-cap refusals count
against the failed-upgrade budget. Budget rejection is saturated: it does not
keep writing larger counters after reaching the ceiling. Global-gate refusals
and unknown-route requests never reach the route object.

There is no application payload queue. The relay reserves outstanding bytes in
attachments before `send()` and closes a session if the reservation would exceed
the window. Closed sessions with unacknowledged bridge-bound data retain their slot and a
bounded reservation in the bridge attachment until the bridge ACKs them. This
prevents repeated connect/send/close cycles from exceeding the aggregate bound.
The bridge must ACK received data even if a close/error follows it. ACKs release
the receiver's direction only; backward/ahead ACKs
close that session. Session IDs cannot select another phone's window. Controls
have fixed sizes and separate rate caps. This bounds data accepted for forwarding
against endpoint acknowledgements; ACKs are endpoint claims, not cryptographic
proof of delivery. End-to-end authentication and credits remain mandatory.
Cloudflare buffers incoming frames before JS receives them, with its own 32 MiB
receive limit; our smaller cap rejects them after dispatch. The relay cannot
prevent platform allocation before dispatch or impose a vendor billing cap.

Token-set stores only hashes plus opaque binding/class/expiry. One binding has
one current hash. Replacing its hash or changing an existing hash's metadata
closes old admissions. Remove deletes all matching registrations and closes
those sessions. Expiry is checked both on admission and forwarding and by an
alarm. Expired registrations occupy slots until the bridge removes them.

## Hibernation and cost decision

Use `acceptWebSocket()` and attachments, with no timers or outbound sockets.
Routing, session ID allocation, rates, expiry and both ACK directions restore
from the attached sockets. An alarm handles pending deadlines and token expiry.
The harness reconstructs the Relay against unchanged attachments and storage,
including a nearly full window. Abrupt platform socket loss still requires a
fresh admission and Noise handshake, not counter reuse.

Pricing/limits checked 2026-11-02 against Cloudflare's primary pages:
[pricing](https://developers.cloudflare.com/durable-objects/platform/pricing/),
[limits](https://developers.cloudflare.com/durable-objects/platform/limits/), and
[hibernation](https://developers.cloudflare.com/durable-objects/best-practices/websockets/).
Paid DO allowances are 1 million requests and 400,000 GB-s/month; incoming
WebSocket messages bill at 20:1. Idle eligible objects incur no duration charge.

For one owner, two always-connected idle phones and one always-connected bridge,
a 30-day model with bridge ping every 15 s and one encrypted ping/reply per phone
plus the four idle-flush ACKs per interval has 9 incoming messages:
1,555,200 messages / 20 = 77,760 DO requests,
plus connection/gate/alarm events. At an assumed 10 ms active time per message:
1,555,200 x 0.01 x 0.128 = 1,990.656 GB-s. One budget row per message is
1,555,200 writes/month. This fits paid included usage: estimate $5/month account
minimum, no incremental DO charge for this idle model. Transfer traffic and
other account usage are additional. This is arithmetic, not measured billing.
For comparison, non-hibernating sockets keep the one route object active:
30 x 86,400 x 0.128 = 331,776 GB-s, independent of three socket count.
Free limits can stop service (notably writes); no vendor spend cap is assumed.
Only local workerd was tested; no Cloudflare plan was exercised in production.

## Reproduce locally

Unit/in-memory tests use Node's built-in runner:

```sh
node --test relay/tests/*.test.mjs
```

The real-runtime check uses pinned tools installed only in scratch. Run from the
repository root, with a writable `TMPDIR`, without any Cloudflare login:

```sh
npm install --prefix "$TMPDIR/relay-tools" --no-audit --no-fund wrangler@4.156.1
RELAY_TEST_ORIGIN="https://192.0.2.1" \
MINIFLARE_MODULE="$TMPDIR/relay-tools/node_modules/miniflare/dist/src/index.js" \
  node relay/tests/workerd.mjs
```

Wrangler 4.156.1 resolves workerd 1.20261101.1. The explicit runner supplies
synthetic tokens and route to an in-memory Miniflare manifest and SQLite-backed
DO bindings. It uses the same Worker modules, not a test-only forwarding path.
Both harness and workerd run two JS Noise endpoints through one bridge, with
interleaved 10 MiB upload/download, matching SHA-256
`44f9296993796e201208c6c245b9515d36b62c87d0be4459ff347bfa054cd527`.
The real runner also withholds ACKs and observes exactly 983,115 forwarded
bytes (15 maximum envelopes), then error, below the 1 MiB window (1,048,576).
