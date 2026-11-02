# Relay Worker (slice 2 of #50)

The Worker joins one bridge and up to four phones per configured route, and
serves the remote page (slice 4, below). No npm runtime dependencies, bridge
process, deployment, or account setup is included. The target is **Workers Free**, using SQLite-backed Durable Objects
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

## Remote page

The Worker also serves the [remote image page](../docs/remote-page.md) from
its static assets (`web/`, see `[assets]` in `wrangler.toml`). It runs first
for every request and serves only the fixed list of page files in `PAGE`, with
the page's CSP and security headers; any other path reaches the route handling
and is refused as before. Page requests count against the Worker request
allowance, not the Durable Object budgets: about a dozen per page load, with no
DO wakeup and no storage. The kill switch also stops the page.

A refused phone admission now closes with a code: 4002 when the route has no
bridge, 4001 when the token is unknown or expired, 4003 when its sessions are
full. Home status is checked first, so a phone is never told its token is
unknown while the bridge is away and may still be re-registering. As a result,
whoever knows a route can learn whether its bridge is connected; the route is
only in pairing links and page connections. A malformed admission or a missed
deadline keeps the generic close.

## Bounds and recovery

Defaults live in `limits.mjs`. Rates use UTC epoch-aligned one-second windows;
per-route budgets use UTC days. Socket attachment counters survive hibernation;
budget reservations and global upgrade counters are committed to SQLite-backed
DO storage before forwarding or granting an upgrade. Storage failures fail closed.

| Limit | Value | On exhaustion / recovery |
| --- | --- | --- |
| Configured routes | At most 100 | Invalid configuration returns HTTP 503 before namespace access |
| Phones per route | 4 | Refuse admission; retry after a slot is fully drained |
| Pending admissions per route | 8 | HTTP 429; retry after admission/timeout releases a slot |
| First-message deadline | 5 seconds | Close; establish a fresh connection |
| Binary envelope | 65,541 bytes | Close offending socket; reconnect with valid sizes |
| Incoming socket traffic | 256 messages and 8 MiB per second | Close offender; reconnect and obey the next window |
| Incoming controls per socket | 32 messages and 4 KiB per second, also subject to total rate | Close offender; reconnect with lower rate |
| Outstanding data per session per direction | 1 MiB | Error and close that session before the next send; fresh session required |
| Aggregate bridge outstanding data | 4 x 1 MiB | Derived from the session cap; each session isolated |
| Application credit window | At most 512 KiB including framing | Endpoint obligation; tested with 492,032-byte bursts |
| Daily incoming binary bytes, shared across routes | 3 GiB | Close offender / reject upgrades; UTC day reset |
| Daily JS events, shared across routes | At most 800,000; reduced for configured bridge count | Close offender / reject upgrades; UTC day reset |
| Daily failed upgrades/admissions, shared across routes | 100 | Refuse further upgrades; UTC day reset |
| Conservative route handler duration, shared across routes | 40,000,000 ms per UTC day (5,120 GB-s) | Refuse work / close sockets; UTC day reset |
| Global upgrade grants | 20 per second, 100 per UTC day | HTTP 429; next second or UTC day reset |
| Budget/token row writes, shared across routes | 10,000 per UTC day | Close offender / reject upgrades; UTC day reset |
| Registered hashes per route | 128 | Close bridge on overflow; reconnect and remove unused bindings |
| Operator kill switch | `KILL_SWITCH` other than literal `false` | Reject upgrades and close sockets on the next JS event; fixed-text auto-response alone is not a kill-switch check |

The daily route budgets are divided evenly (rounded down) among configured
routes; adding routes does not multiply allowance. The global upgrade budget
is shared without division. `budget.mjs` durably reserves capacity in batches
before admission: 128 events, 8 MiB, 4 failures, 32 writes and 124,000 ms of
conservative duration. Reservations are clamped to the corresponding daily cap.
Budget reservation writes and each token-registry mutation charge the write
budget. Global gate and alarm/connection metadata add the bounded overhead below.

Live usage is held in memory and copied to all socket attachments. Hibernation
restores the latest snapshot, so waking after each transfer burst does not
write each event to SQLite. A batch refill is persisted before any operation
can exceed the durable reservation. **Undercount after eviction is zero**. If
all sockets are lost, unused reservations are forfeited: overcount is less than
128 events, 8 MiB, 4 failures, 32 writes, and 155 seconds of duration per such
loss. Repeated losses may stop service early; they cannot reopen spent capacity.
There is no periodic budget alarm or timer, active or idle. The existing alarm
is set only for pending admission deadlines or an admitted session's expiry.

Duration is conservatively reserved as 31 seconds before each route callback,
covering `blockConcurrencyWhile`'s documented 30-second timeout. At the next
event, its fresh I/O timestamp bounds the previous callback; retain the smaller
of that gap and 31 seconds, deliberately including idle gaps. Do not refund
using an in-callback CPU clock: Cloudflare freezes `Date.now()` between I/O.
Bridge auto-response does not invoke this guard. Long gaps between actual JS
events charge at most 31 seconds, not the entire idle gap. The guard is shared
across routes and stops new work before the cap.
An event spanning UTC midnight is charged to its start day (at most 31 seconds
per active route of rollover skew). Clock/runtime billing details still need
production validation. See [timer semantics](https://developers.cloudflare.com/workers/runtime-apis/performance/)
and [concurrency timeout](https://developers.cloudflare.com/durable-objects/api/state/#blockconcurrencywhile).

Daily events include upgrade attempts reaching the route and incoming socket
binary messages, including controls and failed admission bookkeeping. Auto-response
text never enters these counters. Byte budgets count
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

Steady idle now has **no phone traffic** and no JS keepalive wakeups. The bridge
sends TEXT `homepool/1/bridge-ping` every 15 seconds and receives TEXT
`homepool/1/bridge-pong` through `setWebSocketAutoResponse()`. Each frame is 22
UTF-8 bytes: 5,760 frames/day in each direction, or 247.5 KiB of text payload (plus WebSocket/TLS/IP overhead).
Conservatively budget 5,760 / 20 = **288 request-equivalents/day per bridge**;
the documented no-wakeup/no-duration path does not require claiming that
incoming request billing is also free. Keepalive adds **0 GB-s, 0 reads and 0
writes** to DO handlers. Setup, reconnect, token changes and expiry are separate.
The simulated idle day verifies zero changes to wake/read/write/alarm counters.
The real workerd runner forces hibernation, exchanges 100 matching text frames,
then verifies attachment-based data routing still works.

A pong is never admission or registration confirmation. The platform matches
this fixed pair on every attached socket, including pending/phone sockets,
before JS runs. This narrowly scoped echo cannot admit or forward anything;
admission deadlines still expire. Other text closes the socket. The exact pair
necessarily bypasses JS rates, budgets and kill-switch checks; it is liveness
only, like a transport response. New configuration disables auto-response when
the constructor observes the kill switch, and JS data events enforce it before
forwarding. An existing idle echo is not evidence that operations are enabled.
Arbitrary echo flooding/rejected ingress remains outside admitted-work bounds.

Phones reconnect lazily on the next action or page visibility return. They ping
only during an outstanding operation, with an operation pong deadline. They
send no idle encrypted ping and do not use bridge text keepalive. See the full
[phone/bridge contract](../docs/off-lan-protocol.md#relay-level-controls).

For **one GiB (1,073,741,824 payload bytes)**, 60 KiB chunks require 17,477 data
frames, with 2,185 eight-chunk credit bursts. Threshold ACKs, credit-burst ACKs
and 15-second operation-only ping/pong are included in the opaque-frame harness
model. It forwards 2 GiB through the actual relay while reconstructing after
every burst. Per GiB: about **30,630 incoming messages, 370 budget writes and
2,185 cold-load reads**. Payload plus framing is about 1.002 GiB; 2 GiB payload
therefore fits the 3 GiB wire cap. This test covers allowance and flow control,
not 2 GiB encryption, a physical phone, or an object store. The encrypted 10 MiB
workerd check remains separate.

Use a rounded allowance of **31,000 incoming messages, 500 writes and 2,300
reads per GiB**. That is 1,550 request-equivalents and, at an assumed 10 ms per
message, 31,000 x 0.01 x 0.128 = **39.68 GB-s/GiB**. Small files add per-operation
metadata; tiny/misbehaving messages hit the event cap rather than guaranteeing
payload capacity. The 3 GiB allowance covers at least 2 GiB of normal chunked
photo payload with framing and setup headroom. No minimum transfer speed or
physical-device result is claimed.

| Per-day workload (one configured route) | DO request-equivalents | GB-s | Rows written | Rows read |
| --- | ---: | ---: | ---: | ---: |
| Free allowance | 100,000 | 13,000 | 100,000 | 5,000,000 |
| Steady idle keepalive | ≤288 | 0 handler duration | 0 | 0 |
| Idle + 1 GiB, excluding setup | ≤1,838 | ≤39.68 at 10 ms/message | ≤500 | ≤2,300 |
| Idle + 2 GiB, excluding setup | ≤3,388 | ≤79.36 at 10 ms/message | ≤1,000 | ≤4,600 |
| Maximum admitted work with auxiliary allowance | ≤42,288 | 5,516.8 handler-reservation bound | ≤12,100 | ≤1,604,000 |

The worst row uses 800,000 JS events / 20 = 40,000 request-equivalents, 288 for
one scheduled bridge, and 2,000 auxiliary invocations including global grants,
route upgrades, alarms and closes. The guard reserves at most 40,000 seconds x 0.128 =
5,120 GB-s across routes; 100 granted gate callbacks x 31 seconds x 0.128 adds
396.8 GB-s. This is **below half of 13,000 GB-s** before the remaining 983.2
GB-s half-quota margin for dispatch/startup, rollover skew and platform overhead.
The write bound is 10,000 budget/token writes + 100 gate writes + 2,000 alarm/
connection metadata writes; `setAlarm` and deletes count as writes. At most two
reads per admitted event plus at most 4,000 auxiliary reads is 1,604,000. Both writes
and reads also stay below half of their Free allowances. The auxiliary allowance covers
up to 100 carried pending sockets, up to 5N admitted bridge/phone sockets, and
100 new daily grants: at N=100 this permits at most 700 closes, 600 deadline/
expiry alarms and 200 gate/route upgrade calls (1,500 invocations), plus margin.

For N configured routes (at most 100), split byte/write/duration/failure budgets
evenly. Also reserve 288 daily request-equivalents per scheduled bridge and
2,000 for auxiliaries. Total JS-event allowance is the smaller of 800,000 and
`20 x (50,000 - 2,000 - 288 x N)`, then divided among routes. Thus planned
compute requests remain **at or below 50,000/day for every allowed N**. A
regression checks the arithmetic for 1, 2, 16 and 100 routes. The bridge is
trusted to follow its 15-second cadence; auto-response cannot enforce that
cadence in JS. Traffic or retries outside this admitted-work model can consume
the remaining account quota.

All GB-s are either a stated normal-use estimate or conservative application
reservation bound, not invoice telemetry. Hibernation and shared quota behavior
still need production validation. The target remains Workers Free; no billing
change, account access or deployment was performed.

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
The real runner also verifies auto-response after forced hibernation and withholds ACKs and observes exactly 983,115 forwarded
bytes (15 maximum envelopes), then error, below the 1 MiB window (1,048,576).
