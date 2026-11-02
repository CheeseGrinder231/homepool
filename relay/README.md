# Relay Worker (slice 2 of #50)

The Worker joins one bridge and up to four phones per configured route. No npm
runtime dependencies, bridge process, page UI, deployment, or account setup is
included. The target is **Workers Free**, using SQLite-backed Durable Objects
(`new_sqlite_classes` for both classes). No billing-plan change is requested.
The wire contract is in [off-lan-protocol.md](../docs/off-lan-protocol.md).

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
| Daily incoming bytes, shared across routes | 256 MiB | Close offender / reject upgrades; UTC day reset |
| Daily events, shared across routes | 60,000 | Close offender / reject upgrades; UTC day reset |
| Daily failed upgrades/admissions, shared across routes | 100 | Refuse further upgrades; UTC day reset |
| Global upgrade grants | 20 per second, 100 per UTC day | HTTP 429; next second or UTC day reset |
| Budget/token row writes, shared across routes | 75,000 per UTC day | Close offender / reject upgrades; UTC day reset |
| Registered hashes per route | 128 | Close bridge on overflow; reconnect and remove unused bindings |
| Operator kill switch | `KILL_SWITCH` other than literal `false` | Reject upgrades and close sockets on their next event; normal bridge keepalive checks within 15 seconds |

The daily route budgets above are divided evenly (rounded down) among configured
routes, so adding routes does not multiply admitted-work allowance. The global
upgrade budget is shared without division. Token mutations reserve two row
writes (budget and token registry) before mutation; other budget events reserve
one. Alarm/connection metadata and the global gate add small bounded overhead
for at most 100 grants per day.

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
Workers Free allows 100,000 DO requests/day (incoming WebSocket messages at
20:1), 13,000 GB-s/day, 100,000 SQLite row writes/day and 5 million row reads/day.
Free limits reset at 00:00 UTC; exceeding a limit stops the affected operations.
Eligible idle objects incur no duration charge.

For one owner, two idle connected phones and an always-connected bridge, each
15-second interval has 9 incoming messages: bridge ping (1), two encrypted
phone ping/reply pairs (4), and four idle-flush ACKs (4). Thus per day:

- 9 x 5,760 = 51,840 incoming messages, or 2,592 billed DO requests.
- Assuming 10 ms active handler time: 51,840 x 0.01 x 0.128 = 66.3552 GB-s.
- One persisted budget row per message: 51,840 row writes, plus setup/alarms.

This corrects the earlier 1,440-request/~37-GB-s estimate, which omitted ACKs.
A **50 MB (50,000,000-byte) upload** needs ceil(50,000,000 / 61,440) = 814 chunks.
For a conservative allowance, count one encrypted application credit per chunk,
8 handshake/metadata/end envelopes, and one relay ACK per received data envelope:
2 x (814 + 814 + 8) = 3,272 incoming messages. This adds 163.6 billed requests,
4.18816 GB-s at 10 ms/message, and 3,272 budget-row writes. Batched credits/ACKs
normally cost less. Even reserving 1 MiB for framing/metadata fits the 256 MiB byte cap. These are arithmetic assumptions, not a device throughput test.

Idle plus that upload totals **55,112 messages, about 2,756 billed requests,
70.54 GB-s and 55,112 budget-row writes/day**, before setup/alarms. This leaves
about 97,244 DO requests and 12,929 GB-s of Free allowance. Even allowing all
100 daily upgrade grants (two DO calls each) and 100 alarm invocations leaves
more than 96,900 requests. The 60,000-event cap leaves 4,888 event slots in this
model; the 75,000 budget/token-write cap leaves room under the separate 100,000
row-write quota for gate/connection/alarm metadata. Hash updates are charged to
that write budget, not hidden by the WebSocket 20:1 compute discount.

At the 60,000-event cap the same 10 ms model uses 76.8 GB-s and at most 3,000
WebSocket-equivalent requests, plus bounded upgrade/setup work. Duration is an
estimate, not a hard elapsed-time meter: slow handlers can consume more. A single
non-hibernating route object would use 86,400 x 0.128 = 11,059.2 GB-s/day;
hibernation provides the intended headroom. No production plan or billing was
tested. The target model has $0 usage cost on Workers Free.

These controls stop **admitted relay work** with headroom. They do not reserve
account quota: unrelated account traffic and abusive HTTP/upgrade rejections
still reach Cloudflare and can exhaust quotas. Multiple routes share the stated
caps. No application can promise remaining platform quota under arbitrary
external traffic; Free hard limits remain the final stop.

Keep the agreed binary ping/pong. The documented hibernation auto-response API
matches a fixed **string** pair on all attached sockets; it does not match this
binary envelope or perform privileged admission/budget checks. A local workerd
1.20261101.1 probe confirmed the same fixed bytes enter the binary handler while
the text frame receives the automatic reply. A future text
keepalive could avoid the 5,760 daily bridge-ping wakeups (7.3728 GB-s under the
10 ms model), but requires an explicit wire/admission decision. It would still
need a bridge-side pong deadline to detect a dead connection. No such change is
made here. See the [auto-response API](https://developers.cloudflare.com/durable-objects/api/state/#setwebsocketautoresponse).

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
