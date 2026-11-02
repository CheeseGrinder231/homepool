# Home bridge

Slice 3 of [#50](https://github.com/CheeseGrinder231/homepool/issues/50).
`homepool-bridge` is a synchronous application composing the pure Noise protocol
and a separate, fixed-bucket S3 data-plane adapter. It does not use Garage's
administration adapter. There is no page UI in this slice.

Build with `cargo build -p homepool-bridge --locked`. Prepare a new private state
directory explicitly with `homepool-bridge init STATE_DIRECTORY`. Init refuses
an existing directory. Run never creates or replaces an identity. A missing,
malformed or mismatched private/public identity stops startup. Keep an independent
private backup of the identity and state together.

The private JSON configuration has exactly these fields (replace placeholders):

```json
{
  "state_dir": "/PRIVATE/bridge-state",
  "relay_url": "wss://192.0.2.1/example",
  "route": "example",
  "bridge_token_file": "/PRIVATE/bridge-token",
  "page_origin": "https://192.0.2.2/",
  "s3_endpoint": "http://localhost:3900",
  "bucket": "homepool-consumer-1",
  "region": "garage",
  "credentials_file": "/PRIVATE/consumer.json"
}
```

Keep config, token and credentials files owned by the running user with mode
0600. State directory mode is 0700. Paths must be absolute. Credentials use the
[existing version-1 consumer format](garage.md#credentials-file-format-version-1);
the configured bucket must match. The bridge token file contains 64 hex digits
and an optional newline. Never put secrets in command arguments, logs or the
repository. HTTPS verifies the host certificate roots. Plain HTTP/WS is accepted
only with an explicitly loopback endpoint, for isolated local tests. No redirects
are followed. Endpoint and bucket are never supplied by a phone.

Start `homepool-bridge run CONFIG`. While it runs:

- `homepool-bridge invite CONFIG` returns a JSON `link`. Its fragment carries
  `route`, `token`, `bridge`, `invitation` and `psk`. Invitations last ten minutes.
  Treat the complete link as a one-device enrollment capability. It contains no
  S3 credentials. The page origin receives none of the fragment in its request.
- `homepool-bridge devices CONFIG` lists device IDs, full public-key fingerprints
  and enrolling invitation IDs. Compare these after pairing.
- `homepool-bridge revoke CONFIG ID` durably removes the device and expires its
  used invitation, removes the corresponding relay bindings and closes its live
  sessions. In-flight storage workers are cancelled. Data already delivered to
  a store or phone cannot be recalled.

These commands use a Unix socket inside the private directory. One process owns
an advisory lock. Command output can include pairing secrets; keep it private.
The example [user service](examples/homepool-bridge.service) has placeholders only
and is not installed automatically.

## Persistence and transfer bounds

State changes use new mode-0600 temporary files, file sync, atomic rename and
parent-directory sync. Enrollment consumes the invitation and stores the device
and its independently random token in the same snapshot before acknowledgement.
A failed durable write stops the process. A used invitation selects IK for only
its original device. Every connection starts new Noise counters.

The relay loop never calls S3. At most four transfer reservations survive across
sessions, including detached workers from closed sessions. Results have one-slot
channels. Uploads reserve their journal entry, daily quota and spool capacity
and allocates the spool file before granting eight 60 KiB chunks. Hashing is incremental. A matching end is
required before PUT. The fixed random key is generated at home; caller-supplied
keys/headers/methods/queries/copy sources and delete are unavailable. GET keys
pass the protocol's #48 validator. List pages have at most 100 items, with a
bounded XML response. Files are at most 50 MiB. Upload quotas are 1 GiB per device
and 2 GiB total per UTC day; private spool capacity is 200 MiB, shared with
whole-file download reservations. Limits live in `src/settings.rs`.

Upload journal entries are scoped by device and random request ID. Repeated
begins are refused. Before store PUT, the complete spool is synced and its hash
is durable. After confirmation, committed state is durable before `put-done`.
Startup checks every pending fixed key with HEAD. Only a matching length becomes
committed; absence or mismatch becomes failed. Store errors abort recovery without
removing spools. Spools are removed only after that reconciliation snapshot is
synced. Committed entries are retained for 30 days from commitment (including
recovery); pruning happens at startup.
An interrupted/failed upload can retain its quota/spool reservation until restart.

Home enforces four sessions, 10-second/256-byte-per-message handshakes, protocol
message caps, eight-chunk windows, four active workers, a 60-second transfer
no-progress limit and a 24-hour session lifetime. Idle authenticated sessions are
not closed for silence. Sessions are visited in turn, sending one download chunk
per visit. Pings do not advance transfer progress. S3 sockets have 30-second
operation budgets. S3 resolution uses at most four background resolver threads; callers
wait only within the operation deadline. Relay resolution runs off the control
loop and follows the host resolver timeout. Relay text keepalive is every 15 seconds with a
10-second pong deadline. Disconnects discard all Noise sessions and reconnect
with exponential backoff and random jitter; token registrations are re-sent.

## Dependencies and validation

New direct crates: tungstenite 0.30 (synchronous WebSocket framing and bounded
buffers), rustls 0.23 with ring (certificate-verified TLS), rustls-native-certs (host
roots), hmac 0.12 (SigV4 with the existing sha2), quick-xml 0.38 (bounded S3 XML),
url 2.5 (structured endpoint validation), time 0.3 (UTC signing/key stamps), and
getrandom 0.3 (OS randomness). Existing snow 0.10, serde, serde_json, httparse and
rustix are reused. No async runtime, AWS SDK, shell S3 client or runtime npm
dependency is added. `deny.toml` is unchanged.

The explicit real-runtime runner uses scratch-installed Wrangler/workerd from
[the relay guide](../relay/README.md#reproduce-locally), the installed Garage
binary in `HOMEPOOL_TEST_GARAGE`, and `CARGO_TARGET_DIR`:

```sh
cargo build -p homepool-bridge --locked
MINIFLARE_MODULE="$TMPDIR/relay-tools/node_modules/miniflare/dist/src/index.js" \
  node crates/homepool-bridge/tests/local.mjs
```

It creates isolated loopback services and credentials. It does not read live
configuration or require a Cloudflare account. Rust tests cover atomic private
state, durable enrollment, used-invitation refusal, paired-device refusals,
request-ID/quota reservation and device-scoped status. The runtime runner checks
two enrollments, invitation-token acknowledgement recovery and wrong-device
refusal, IK reconnects, interleaved 10 MiB PUTs, listing, checksum GETs,
a kill after store confirmation but before journal commitment, recovery/status,
relay replacement with fresh token registration, short-burst receipt cleanup
under session churn, and live-session revocation.

Local evidence uses JS endpoints and loopback WS, not physical phones or a public
WSS deployment. Unproven: target-phone persistence/backgrounding and 50 MiB
transfers, production WSS/HTTPS certificate paths, Cloudflare deployment/billing,
and power-loss/filesystem fault injection. The quota refusal uses a synthetic
prior durable usage entry rather than uploading a GiB. No live units, config,
credentials, Garage, Caddy or Cloudflare account are used.
