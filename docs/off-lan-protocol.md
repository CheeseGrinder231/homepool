# Remote image protocol, version 1

Slice 1 of [#50](https://github.com/CheeseGrinder231/homepool/issues/50).
This defines bytes and pure session/transfer state only. It opens no sockets,
reads no credentials, performs no storage operations and authorizes no device.
The example endpoint is an offline test tool, not a bridge executable.

## Cryptography and API contract

Enrollment is `Noise_XKpsk3_25519_AESGCM_SHA256`; reconnect is
`Noise_IK_25519_AESGCM_SHA256`. Phone is initiator. The bridge static public key
is pinned before either handshake. Enrollment requires exactly 32 PSK bytes.
Prologue bytes are UTF-8 `homepool/1/<mode>/<route>`, where mode is `enroll` or
`reconnect`, and route is 1–64 ASCII letters, digits, underscores or hyphens.
The separators cannot occur in these components. There is no mode fallback.

Noise follows [revision 34](https://noiseprotocol.org/noise.html): AESGCM uses
four zero bytes followed by the 64-bit **big-endian** nonce, 128-bit tags,
SHA-256, and Noise HKDF (HMAC expansion, including the empty-input Split).
PSK patterns mix every ephemeral public key into the chaining key, in addition
to mixing the PSK at token psk3. All handshake payloads must be empty.
No application operation, token or metadata is put into a handshake payload.

JS `Session.create` accepts a non-extractable X25519 private `CryptoKey` and
its separate raw public key. It never exports a private key. `vectorKey` imports
public test fixtures only. `NoiseCore` is an internal testable primitive that
permits published vectors' payloads; applications must use `Session`.
Per-session async operations share one queue. Rust requires `&mut Session`.
Every state, size or authentication error permanently closes that session.
A new connection needs a new handshake; neither side restores counters.
Transport messages use empty associated data, as specified by Noise.

`remoteStatic` / `remote_static` reports cryptographic identity, **not** permission.
The bridge must check a reconnect's static key against its device store after
message 1 and finish the handshake before accepting transport operations.
Enrollment's durable, atomic invitation checks happen after message 3, before
an encrypted `enrolled` acknowledgement. The pure protocol does neither check.
A replayed IK message 1 can pass static-key checks but cannot produce transport
for a fresh responder ephemeral. This is why operations never occur in payloads.

## Application wire format

One application record is one Noise plaintext; one Noise ciphertext is one
binary WebSocket message (inside one relay envelope). Plaintext max is 65,519
bytes; ciphertext max is 65,535. Never concatenate ciphertexts or split them
across WebSocket messages. Chunk data max is 61,440 bytes, leaving header room.

`protocol/schema.json` is the normative ordered table. `schema.mjs` is its
browser-loadable mirror, checked for equality by tests. Each record starts with
version byte `1`, then a one-byte type ID: the table's 1-based index. Fields
follow in table order, without field names. No optional or additional fields,
unknown IDs, invalid UTF-8, trailing bytes or other versions are accepted.
JSON is used by the test harness only, not on the wire.

| Field kind | Encoding and bound |
| --- | --- |
| length | unsigned 64-bit big endian, at most 2^53−1 |
| count | unsigned 32-bit big endian |
| page | unsigned 32-bit big endian, 1–100 |
| credit | unsigned 32-bit big endian, 1–16 |
| all text | unsigned 16-bit byte length then strict UTF-8, no C0 or DEL |
| id | exactly 32 lowercase hex characters (128 random bits) |
| token, hash | exactly 64 lowercase hex characters (256 bits) |
| key | at most 512 bytes; `YYYY-MM-DD/HHMMSS-<26 lowercase base32>-<safe name>` |
| name | 1–255 bytes |
| mime | 0–127 ASCII bytes; empty means unspecified |
| cursor | 0–1024 bytes, opaque; empty starts/ends pagination |
| code | 1–32 lowercase ASCII letters or hyphens |
| data | unsigned 16-bit length then 1–61,440 raw bytes |
| items | unsigned 16-bit count, 0–100 entries; each is key, length, MIME |

Keys follow #48: the 26-character tag encodes 128 bits in lowercase RFC 4648
base32 without padding, with canonical final padding bits. Safe names are at
most 73 ASCII letters, digits, dots, underscores or hyphens; neither end is a dot
or hyphen.
The date/time shape is syntactic; this layer does not interpret calendar dates.
Names are metadata, never paths or chosen object-store targets. MIME metadata
never permits executing HTML or SVG. File size, quotas and spool limits must be
lower deployment bounds; 2^53−1 is only the interoperable integer ceiling.

Types 20–23 (`services`, `services-list`, `service-get`, `refused`) carry the
read-only service tabs of #59; see [service tabs](services.md). Their field
kinds are `services` (u16 count, at most 16 entries of `service` then `title`),
`service` (1–32 lowercase letters, digits or hyphens), `title` (1–40 characters)
and `reason` (one of five fixed words). Vectors: `protocol/service-vectors.json`.

The discriminant is `type` in the JS API; MIME fields are called `mime`.
List items use `type` for their MIME (they have no message discriminant).
The Rust codec takes/returns `serde_json::Value` but validates the entire closed
schema. Binary data appears as byte arrays in that API and `Uint8Array` in JS.

## Operation and transfer contract

Request IDs correlate operations within a session. The page creates a fresh
random ID for each upload and binds it permanently to that File, as #50 requires.
Only one upload under `(device, ID)` may begin, including across sessions.

- `list(id,cursor,limit)` → `list-page(id,cursor,items)`. A page has at most the
  requested limit, never more than 100. Empty response cursor means complete.
- `get(id,key)` → `get-head(id,length,mime)`, ordered `chunk(id,index,data)`,
  then `get-end(id,length,count)`.
- `put-begin(id,name,mime,length)` → bridge reserves and journals before granting
  credits; ordered chunks follow, then `put-end(id,length,count)`.
  After durable commitment: `put-done(id,key)`.
- `put-status(id)` → exactly one of `pending(id)`, `failed(id)`, `unknown(id)`,
  or `committed(id,key,name,mime,length,hash)`. Hash is SHA-256 of stored bytes.
  Status is scoped to the authenticated device. The upload journal and 30-day
  retention belong to the bridge, not this codec.
- `error(id,code)` reports a request failure; code carries no arbitrary content.
- `credit(id,chunks)` grants additional chunks for that transfer, never bytes.
  Receiver grants at most 16 outstanding chunks; sender consumes one per chunk.
  Downloads and uploads both use credits. Metadata/end records consume no credit.
  Credits apply only after the matching begin/head; they cannot migrate sessions.
- `ping(id)` → `pong(id)` within the authenticated phone session. These records
  do not count as transfer progress.
- `enrolled(id,token)` acknowledges the enrollment, carrying device ID and token.
  Device ID is bridge assigned. The record follows durable enrollment only.

Chunk indices start at zero and increment by one. `Transfer` validates correlation,
order, total length and end count and closes on any error or completion. It does
not buffer file bytes. Empty files use no chunks and an end count of zero.
No Blob is released and no upload committed before a valid end record.
The caller tracks operation direction, unique active IDs, credits, bounded queues,
deadlines and authorization. This layer cannot detect a silently dropped final
message without the caller's no-progress deadline or connection closure.

## Relay and admission

Every post-admission binary message, including phone messages, has a six-byte
header: version `1`, kind byte, unsigned 32-bit big-endian session ID. Kinds:
`0=data`, `1=open`, `2=close`, `3=error`, `4=token-set`, `5=token-remove`,
`8=ack` (6/7 are unused). Data/open/close/error session IDs are nonzero, relay assigned,
and scoped to one bridge connection. They are never device identities. Never
reuse an ID while the bridge connection exists; reconnect before exhaustion.
Close/error contain exactly the header, with no human-readable reason.
Open has a four-byte unsigned big-endian token binding after the header.
Data contains one opaque Noise message, up to 65,535 bytes. Relay cap: 65,541.

The relay sends `open` before a phone's first data. It overwrites/checks the
phone's assigned routing ID; a phone cannot select another session. A fresh
bridge socket invalidates all old sessions. Unknown/stale IDs cannot allocate
handshake state. Open allocation limits and admission deadlines belong to slice 2.
A phone learns its assigned ID from `open`; it sends no data before that.

First socket message is admission, never an envelope: exactly 34 binary bytes,
version `1`, role `0=phone` or `1=bridge`, then the 32 raw token bytes. No route,
PSK, key or content appears here. Route is connection context; token is never a
URL parameter or logged. The relay forwards nothing until admission succeeds.
Tokens admit relay use only. Token hashes and invitation/device binding are
bridge/relay policy. Every reconnect sends admission again.
The bridge also sends `Authorization: Bearer <bridge-token>` on the HTTP upgrade.
The token is the same 64 lowercase hex digits from its existing token file.
The Worker validates it before granting a separate bridge upgrade allowance;
phone failures or allowances cannot consume that reservation. It strips this
header before forwarding to the route object. Binary admission remains required
and unchanged. A client-supplied internal bridge marker grants no privilege.
Legacy bridges without the header still use the public admission path and its
shared phone allowances; update the bridge to gain isolation.
A refused phone admission closes the socket with code 4002 when the route's
bridge is not connected, 4001 when the token is unknown or expired, and 4003
when the route's sessions are full, checked in that order; other failures use
the generic close. The page uses these to explain the refusal.

## Relay-level controls

The type numbers, positional field encoding, bounds, pagination response,
request correlation, relay ID lifetime, admission layout and credit record above
fill omissions in #50; they do not add operations or change either Noise pattern.
No aggregate page can exceed the plaintext cap even if all individual fields fit.
The page must reduce its item count when long keys would exceed that cap.

Session zero is relay-level and is forbidden for data/open/close/error/ack.
Token controls are accepted only from the admitted bridge. All integers below
are unsigned big-endian; fixed payload sizes exclude the six-byte header.

| Kind | Payload | Bytes |
| --- | --- | --- |
| token-set (4) | SHA-256 token hash (32), class (u8: 0 invitation, 1 device), binding (u32), expiry (u64 Unix seconds; 0 means none) | 45 |
| token-remove (5) | binding (u32) | 4 |
| ack (8), nonzero session ID | cumulative data-envelope bytes received on this session (u64) | 8 |

The relay stores only token hashes in Durable Object storage, never mints tokens,
and closes all sessions admitted with a removed binding. Open carries the binding
of the admitting token; only the bridge assigns its meaning and permissions.
After binary admission, the bridge sends the exact TEXT frame
`homepool/1/bridge-ping` every 15 seconds; hibernation auto-response returns the
exact TEXT frame `homepool/1/bridge-pong`. The bridge allows one outstanding ping,
requires its pong within 10 seconds, and closes/reconnects with backoff and
jitter when late. A pong proves only that the relay connection responds: never
bridge admission, token registration persistence, end-to-end identity or storage
availability. It carries no storage operation and wakes no DO handler.

The auto-response API applies to all sockets attached to the route, including
pending/phone sockets. The exact fixed-string echo is a non-authoritative
liveness exception outside JS admission/rate/budget handling. A pending socket
still needs binary admission before its deadline and cannot forward data;
matching text cannot install tokens, replace a bridge or acquire a session.
Other text frames are rejected. Scheduled keepalive traffic is included
conservatively in the request allowance; arbitrary echo flooding cannot be
bounded by JS without defeating zero-wakeup auto-response.

Phones send **no idle pings**. An idle phone may lose its socket silently; on
the next user action it reconnects lazily as needed, sends fresh admission and
completes a fresh Noise handshake. On page visibility return, treat the old
transport as stale and reconnect with fresh admission and Noise.
Never resume old transport counters. Only while an operation is outstanding
may a phone send the encrypted application `ping` every 15 seconds, with a
10-second pong deadline. A late pong interrupts the operation and reconnects;
use `put-status` to resolve uncertain uploads. No-progress deadlines still
require file bytes/credits, so ping does not extend a stalled transfer. An idle
phone does not send the bridge's text keepalive. Phone activity/visibility UI
and bridge reconnect loops are contracts for the subsequent slices.
The codec accepts the entire u64 expiry range (JavaScript uses BigInt), without
rounding through a JavaScript Number. Shared exact vectors are in
`protocol/relay-vectors.json`, including both classes and maximum integer values.

ACK is endpoint-to-relay only: a phone acknowledges its assigned session, and
the bridge acknowledges each session independently. Each direction starts at
zero on open and counts the entire forwarded data envelope, including its
six-byte header (Noise handshake messages count too). ACK when at least
64 KiB is unacknowledged, or flush once after a file/handshake burst or before
closing. Individual small keepalives do not trigger an idle-flush ACK.
Duplicate ACKs are idempotent; decreasing totals or totals beyond bytes forwarded close the session.
Controls never count toward the data window and are rate-limited separately.

Before every data send the relay reserves `forwarded - acked + envelope length`.
If that exceeds RELAY_WINDOW (1 MiB), it sends no data and errors/closes that
session. It never queues the excess. The shared bridge's aggregate data bound
is session cap times window (four sessions: 4 MiB). Closed sessions keep their
slot while bridge-bound bytes remain unacknowledged; the bridge must ACK them
even if close/error follows the data. These bounded reservations survive
hibernation and cannot allocate new sessions. Application credit windows
must be at most half RELAY_WINDOW (512 KiB), including all schema/Noise/envelope
overhead, to leave headroom for ACK scheduling. The integration tests use eight
60 KiB chunks per burst (492,032 envelope bytes) and flush at transfer-burst
boundaries. Threshold plus application window is 576 KiB, below the 1 MiB relay
window. Operation-only encrypted ping/pong traffic uses the same threshold.
A 24-hour regression sends only bridge text keepalive and verifies zero JS
wakeups, reads, writes, alarms or phone traffic. Binary controls are fixed-size
and separately capped; ACKs cannot
allocate stale sessions or select another phone. The relay trusts cumulative
receipt claims only for flow control, never for identity or storage permission.

## Evidence and limits

`protocol/cacophony.json` contains exact IK and XKpsk3 AESGCM/SHA256 vectors,
plus the closest unmodified XK, from snow v0.10.0's
[cacophony fixture](https://github.com/mcginty/snow/blob/v0.10.0/tests/vectors/cacophony.txt).
These are public deterministic test keys, not credentials. JS and Rust check
both exact patterns' ciphertexts, payloads and handshake hashes. Community
handshake payloads are permitted only in the primitive/vector tests.

`web/protocol/tests/interop.test.mjs` starts real Rust child processes over pipes:
both patterns, both JS roles, replay rejection, shared schema/refusals and
boundaries. Its memory relay interleaves a 10 MiB + 7 byte upload from phone A
and download to phone B, through one Rust endpoint with two independent sessions,
checking all three SHA-256 values. No socket, relay service or object store runs.
Rust/JS negative tests cover wrong key at message 1, PSK at message 3, altered,
dropped/reordered transport, poisoned sessions, and 65,519/65,520 byte boundaries.

Run `cargo test -p homepool-protocol`, then `node --test web/protocol/tests/*.test.mjs`
with `CARGO_TARGET_DIR` set to the same build directory. The process test builds
the endpoint offline; fetch/build dependencies first. Existing page tests remain
under `web/images/tests`. There are no npm dependencies or build step.

Unproven here: this implementation on either target phone; browser persistence of wrapped keys;
real Cloudflare deployment/billing; live enrollment/revocation/durable journal;
S3/storage behavior; browser backgrounding and memory; encrypted 50 MB phone
transfers or real-world throughput. These require later slices and device runs.

The #52 regressions preserve leading U+FEFF in text fields (UTF-8 decoding does
not treat it as a removable BOM). Shared tests cover U+FEFF-only names, leading
U+FEFF cursors, non-ASCII names and consistent text refusals. Live transfer tests
reject duplicate/skipped indices and wrong end counts. JavaScript tests exercise
both cipher directions at nonce 2^64-2 and the reserved 2^64-1, plus both session
patterns with a non-extractable private key restored by `unwrapKey`.

Slice 2's Worker and tests are under `relay/`. Its in-memory harness and pinned
local workerd runner both move an encrypted, interleaved 10 MiB upload/download
through the actual relay code. See [relay notes](../relay/README.md) for limits,
hibernation cost arithmetic, exact local commands and remaining deployment gaps.
