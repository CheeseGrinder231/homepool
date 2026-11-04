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
messages. The relay emits no logs and observability is disabled. The platform
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

## Bounds, recovery and billing

The target remains Workers Free, with SQLite-backed objects. No account or
billing change is required. Pricing and limits checked 2026-11-04:
[Workers pricing](https://developers.cloudflare.com/workers/platform/pricing/),
[Workers limits](https://developers.cloudflare.com/workers/platform/limits/),
[DO pricing](https://developers.cloudflare.com/durable-objects/platform/pricing/),
[DO limits](https://developers.cloudflare.com/durable-objects/platform/limits/).

The Worker accepts at most 100 configured routes. A public route has four phone
sessions, eight pending admissions, a five-second admission deadline, 65,541-byte
binary frames, and per-socket limits of 256 messages / 8 MiB each second and
32 controls / 4 KiB each second. Outstanding unacknowledged data stays below
1 MiB per session per direction, including closed sessions awaiting receipts.
There is no application payload queue. Endpoints must retain Noise authentication
and credits; relay ACKs are delivery claims, not authentication.

Phone budgets are shared across routes (integer division): 3 GiB incoming wire
bytes, 570,000 message-equivalent units, 7,000 budget/token row writes,
16,000,000 active milliseconds and 100 failed admissions daily. An incoming
WebSocket binary message costs one unit; an upgrade costs 20. HTTP upgrades,
alarms and closes also have the separate auxiliary allowance below. A singleton
gate grants at most 1,000 public upgrades daily, at most 20 each second.

The bridge supplies its existing token in an **Authorization: Bearer** header
as well as its unchanged binary admission. This handshake addition is necessary:
the old first-message-only credential arrives after the shared HTTP upgrade
allowance has already refused the connection. There is no configuration change.
The Worker validates the header, strips it, strips any caller-supplied internal
bridge marker, and sets its own marker before forwarding. Phones cannot request
the reserved lane. Each route has eight separately gated bridge upgrades daily;
a connected bridge or pending authenticated claimant returns 409. Bridge
admission bypasses phone failures, pending slots and daily guards.

Bridge session-zero controls have independent shared budgets: 130,000 units,
30,000 writes and 2,000,000 active milliseconds, divided
among routes, plus 1 MiB incoming control bytes per route. Re-registering identical token metadata writes nothing. At 100
routes this admits eight reconnects with 128 registrations each (1,192 units,
including eight upgrades and admissions, below 1,300, allowing at most 105 additional units forfeited across seven socket losses). Actual mutations stay
within the independent write allowance; saturation refuses the mutation without
closing the bridge. Bridge data uses phone budgets. Exhaustion stops phone work
and closes phone sessions, preserving the bridge and its auto-responses. Full
receipts for retired sessions release existing state once, even after saturation,
so exhausted guards cannot strand next day's session slots.

Budgets reserve batches before use: 128 units, 8 MiB, four failures, 32 writes
and 4,000 milliseconds for phone work. Bridge control batches instead reserve
16 units, 4 KiB, one failure, eight writes and 100 ms; using the phone's 8 MiB
batch would forfeit the bridge's entire smaller byte allowance on restart.
Attachments retain live usage through hibernation;
losing all sockets forfeits unused reservations rather than reopening capacity.
There are no periodic budget alarms. Admission/expiry alarms remain bounded by
new and carried sockets. Counters and gate grants reset at UTC midnight.
When the bridge's own eight-upgrade allowance is exhausted it retries with
back-off and recovers after the UTC reset; phones cannot spend this allowance.

### Active duration and local reservation evidence

The old meter settled the previous callback at the **next event**, charging
idle gaps as active time. The new meter reserves 2,000 ms, then settles the
callback before returning to hibernation eligibility. A one-millisecond timer
completion supplies a fresh I/O clock timestamp because Date.now is frozen
between I/O in workerd. Its wait is included in charged duration; no later idle
gap is included. Matching bridge text auto-responses invoke no JS and incur no
additional duration. Outgoing responses are free. The model conservatively
retains the incoming text messages at 20:1, as in the previous budget. The fixed pair is liveness only;
it bypasses admission, rates and kill-switch checks and grants no authority.

In local workerd 1.20261101.1, a profiled run of the existing encrypted,
interleaved 10 MiB transfer / hibernation / slow-receiver runner observed **543
callbacks, 1,646 ms total and 29 ms maximum**, including completion checkpoints.
A second final-candidate run under concurrent local verification measured 543
callbacks, 3,378 ms total and a 365 ms maximum. The 2,000 ms reservation is more
than five times the larger observed maximum. This is
an empirical local bound, not a Cloudflare storage-latency SLA: the platform
provides CPU limits but no general HTTP/DO wall-time limit. Runtime pauses or
storage stalls can exceed it. The guard is an admitted-work bound under this
stated measurement assumption; it is not an invoice hard cap. Revalidate the
reservation on workerd changes and production latency before relying on paid
plan cost protection. No Cloudflare login or deployment was used for evidence.

### Free-plan arithmetic

For N routes, 1 <= N <= 100, global granted upgrades U <= 1,000 + 8N <= 1,800.
The auxiliary allowance is 14,000 request-equivalents, covering gate and route
upgrade calls, alarms, closes and bounded retired-session receipts. At most
1,400 sockets carry from the prior day (four phones, one bridge and up to nine
pending per route); adding 1,800 new grants gives at most 3,200 close callbacks
and 3,200 deadline/expiry callbacks. Two DO calls per grant plus these callbacks
and at most 1,400 carried/full retirement receipts total 11,400, below 14,000.
Each alarm/connection scheduling call writes at most once: 2U + 3,200 + 3,200
<= 10,000 auxiliary row writes. Two cold reads per counted message plus twice
the auxiliary allowance overstates actual budget/token reads.

| Worst modeled day, all routes | Maximum | Free allowance | Free fraction |
| --- | ---: | ---: | ---: |
| Worker requests, U + 1,000 page loads at 14 requests | 15,800 | 100,000/day | 15.8% |
| DO requests, route units/20 + 6,500 bridge + 14,000 auxiliary + 288N keepalive | 50,000 | 100,000/day | 50% |
| DO active duration: (18,000 + 2 x 14,000 + 2 x (2N+1)) seconds x 0.128 | 5,939.456 GB-s | 13,000 GB-s/day | 45.69% |
| SQLite row writes: 7,000 + 30,000 + U + 11,000 | 49,800 | 100,000/day | 49.8% |
| SQLite row reads: 2 x 700,000 + 2 x 14,000 + 1,000 | 1,429,000 | 5,000,000/day | 28.58% |

Duration includes 18,000 guarded route/bridge seconds, one measured reservation
for every auxiliary invocation (including gates, alarms, closes and retired
receipts), and one reservation of rollover skew per object: N route objects,
N bridge gates and the public gate. Row figures also reserve 1,000 reads and
writes for callbacks spanning UTC midnight. A route callback has at most five
row writes and a gate callback at most one, below this reserve even at N=100. Lifecycle cleanup
runs even when forwarding guards are spent; otherwise a closed bridge attachment
could prevent every replacement bridge from being admitted.
Rows include reservation rows themselves, token mutations, gate rows and alarm
scheduling/deletes. Retained state is two budget rows, a bounded token registry
(128 hashes per route) and bounded gate rows; far below 2.5 GB of the Free 5 GB
storage allowance. No payload is retained. No KV namespace, R2 or D1 is used.
Phone units are the smaller of 570,000 and
`20 x (50,000 - 14,000 - 6,500 - 288N)`, divided among routes. This preserves
room for every configured bridge's incoming keepalive; its outgoing auto-response
and duration stay free. The request, duration and row arithmetic is checked
for N=1,2,16,100.

A normal bridge-connected 24-hour day has 5,760 incoming fixed text keepalives
and zero JS keepalive callbacks, rows or duration. The photo harness measured
about 30,630 incoming messages, 364 writes and 2,185 reads per GiB, including
ACKs and credits. Two GiB fits the 3 GiB wire cap. At an estimated 10 ms/message,
it uses 79.36 GB-s; the real 10 MiB profiled run above is distinct from that
estimate. App switching may reconnect phones without consuming bridge grants.

Unknown-route traffic, refused ingress, exact echo floods, excess page loads and
other account users remain outside the admitted-work model and can consume the
platform quota. Free stops operations at its limits. The application cannot
limit billed ingress before Cloudflare receives it. This is the same scope as
the prior model; it must not be described as a global account billing cap.

Bridge back-off starts at one second, doubles to 60 seconds, and retains up to
255 ms jitter. Registration success alone does not reset it. A connection must
remain up for 60 seconds (four 15-second keepalive intervals) before a subsequent
loss resets retries. Stderr reports up/down, numeric HTTP refusal status or close
code, and retry delay; it never prints response bodies, reasons, URLs or secrets.

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

The incident regressions also run the actual bridge, without a store or login:

```sh
export MINIFLARE_MODULE="$TMPDIR/relay-tools/node_modules/miniflare/dist/src/index.js"
export BRIDGE_TEST_BINARY="$TMPDIR/target/debug/homepool-bridge"
CARGO_TARGET_DIR="$TMPDIR/target" cargo build -p homepool-bridge
node relay/tests/lockout-workerd.mjs lockout
node relay/tests/lockout-workerd.mjs guard
node relay/tests/lockout-workerd.mjs spiral
node relay/tests/lockout-workerd.mjs hostile
```

`lockout` reduces only the duration cap to 32 seconds and leaves a real idle
gap; main refused upgrades afterward. `guard` sets the phone units to zero;
the bridge still connects and stays up. `spiral` injects a loss 100 ms after
admission: main reconnects about every 1.2 seconds, while the candidate backs
off 1/2/4/8 seconds plus jitter. Fixtures are injected into an in-memory
manifest, never deployed. Existing encrypted transfer and browser runners remain
separate. Existing stored counters are retained until UTC reset on upgrade;
this change does not erase already-spent daily allowances.
