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
budget reservations and global upgrade counters are committed to SQLite-backed
DO storage before forwarding or granting an upgrade. Storage failures fail closed.

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
| Conservative route handler duration, shared across routes | 90,000,000 ms per UTC day (11,520 GB-s) | Refuse work / close sockets; UTC day reset |
| Global upgrade grants | 20 per second, 100 per UTC day | HTTP 429; next second or UTC day reset |
| Budget/token row writes, shared across routes | 10,000 per UTC day | Close offender / reject upgrades; UTC day reset |
| Registered hashes per route | 128 | Close bridge on overflow; reconnect and remove unused bindings |
| Operator kill switch | `KILL_SWITCH` other than literal `false` | Reject upgrades and close sockets on their next event; normal bridge keepalive checks within 15 seconds |

The daily route budgets are divided evenly (rounded down) among configured
routes; adding routes does not multiply allowance. The global upgrade budget
is shared without division. `budget.mjs` durably reserves capacity in batches
before admission: 128 events, 8 MiB, 4 failures, 32 writes and 124,000 ms of
conservative duration. Reservations are clamped to the corresponding daily cap.
Budget reservation writes and each token-registry mutation charge the write
budget. Global gate and alarm/connection metadata add the bounded overhead below.

Live usage is held in memory and copied to all socket attachments. Hibernation
restores the latest snapshot, so even waking after every idle ping burst does
not write each event to SQLite. A batch refill is persisted before any operation
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
Thus an idle bridge at 15-second intervals consumes almost a full day's wall
clock in this **guard**, while its actual hibernating billing estimate is much
smaller. The guard is shared across routes and stops new work before the cap.
An event spanning UTC midnight is charged to its start day (at most 31 seconds
per active route of rollover skew). Clock/runtime billing details still need
production validation. See [timer semantics](https://developers.cloudflare.com/workers/runtime-apis/performance/)
and [concurrency timeout](https://developers.cloudflare.com/durable-objects/api/state/#blockconcurrencywhile).

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

For one owner, two idle connected phones and an always-connected bridge, a
15-second interval has 5 incoming messages: one bridge ping and two encrypted
phone ping/reply pairs. Each encrypted keepalive is a 58-byte data envelope.
A receiver ACKs at **64 KiB unacknowledged**, not after each small ping. In a
24-hour model there are only 20 threshold ACKs across four session directions:
28,800 + 20 = 28,820 incoming messages / 20 = 1,441 billed DO requests.
At 10 ms active handler time, 28,820 x 0.01 x 0.128 = 36.8896 GB-s/day.
The test reconstructs the relay after every 15-second burst and measures
**870 row writes, 5,763 reads, zero alarms and 20 ACKs** for the full day.
Those counts include the route's synthetic setup; allow under 1,000 writes
with real upgrade/gate metadata. This replaces the former per-message writes.

A **50 MB (50,000,000-byte) upload** has 814 chunks of at most 61,440 bytes,
102 eight-chunk credit bursts and 8 handshake/metadata/end envelopes in this
model. Two full chunks cross the 64 KiB threshold; allowing prior unacknowledged
keepalive bytes, burst/end flushes and the reverse credit stream, budget at most
410 relay ACKs: 814 + 102 + 8 + 410 = 1,334 incoming messages. This adds
66.7 billed requests and 1.70752 GB-s at 10 ms/message. Conservatively allow
50 extra reservation/metadata writes and 1,500 reads (even a cold reconstruction
after every new message). Framing plus metadata fits a 1 MiB allowance, so the
upload remains well inside the 256 MiB byte cap. This is arithmetic, not a
50 MB target-device throughput result.

| Per-day workload | Billed DO requests | GB-s | SQLite rows written | SQLite rows read |
| --- | ---: | ---: | ---: | ---: |
| Free allowance | 100,000 | 13,000 | 100,000 | 5,000,000 |
| Idle model, including setup allowance | <1,460 | <37 | <1,000 | <6,000 |
| Idle plus one 50 MB upload | <1,530 | <39 | <1,050 | <7,500 |
| Maximum admitted work under own caps, with auxiliary allowance | ≤4,200 | 11,916.8 handler reservation bound | ≤11,100 | <125,000 |

The last row derives from 60,000 incoming events / 20 = 3,000 request-equivalents,
100 global upgrade grants x 2 DO calls, and allowance for 1,000 auxiliary
alarm/close invocations. The route duration cap is 90,000 seconds x 0.128 =
11,520 GB-s, plus 100 granted gate callbacks x 31 seconds x 0.128 = 396.8 GB-s.
The write bound is 10,000 budget/token writes, 100 gate writes and up to 1,000
alarm/connection metadata writes (`setAlarm` counts as a row write; deletes are
also counted). Two reads per incoming event, plus auxiliary loads, stay under
125,000. Batches can only reduce admitted work; no eviction grants free usage.
This leaves at least 95,800 requests, 88,900 writes and 4,875,000 reads of
headroom in that admitted-work model. Ordinary helper/setup counts fit the
auxiliary allowance; platform retry,
startup/dispatch overhead and rollover skew consume part of the remaining
1,083.2 GB-s margin. These are application accounting bounds, not a promise of
exact invoice metering. The low normal-use GB-s rows assume hibernation and
10 ms/message; the conservative duration guard makes no 10 ms assumption.

There is no subscription cost assumed: the target is Workers Free. These caps
bound **admitted relay work** with headroom, not arbitrary account traffic.
Rejected requests already reaching Cloudflare, repeated platform failures and
unrelated account workloads may still exhaust quota. No application can reserve
the remaining account quota. Only local workerd has been tested; actual billing,
platform hibernation and timer behavior remain unproven on a real deployment.

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
