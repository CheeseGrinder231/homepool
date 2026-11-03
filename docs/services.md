# Read-only service tabs

Built from [#59](https://github.com/CheeseGrinder231/homepool/issues/59).
The [remote page](remote-page.md) can show tabs beside Photos with read-only
status from other services the owner runs at home. A document travels through
the same pairing, the same end-to-end encrypted session and the unchanged
[relay](../relay/README.md). Each service keeps its own server, data and
lifecycle; the bridge only reads one fixed URL per service.

## Catalog

The catalog is an optional `services` list in the bridge's private
configuration ([bridge](bridge.md)). Without it, the page shows no tabs. Use
placeholders like these; a real catalog stays private.

```json
"services": [
  { "id": "status", "title": "Status", "url": "http://127.0.0.1:PORT/view.json", "devices": "all" },
  { "id": "work", "title": "Work", "url": "http://[::1]:PORT/work.json", "max_bytes": 65536,
    "devices": ["DEVICE_ID"] }
]
```

- `id`: 1–32 lowercase letters, digits or hyphens, unique. At most 16 services.
- `title`: the tab's name, 1–40 characters, no control characters.
- `url`: plain HTTP to the loopback literal `127.0.0.1` or `[::1]`, with an
  explicit port and a fixed path; no user, query or fragment, and written the
  way the URL parser would write it (no `..`, no `:080`). Anything else stops
  the bridge at startup with a message naming the service.
- `max_bytes`: default 262,144 (256 KiB), at most 1,048,576 (1 MiB).
- `devices`: `"all"`, or the device IDs from `homepool-bridge devices CONFIG`.
- Unknown fields, such as a method or headers, are refused.

The catalog is read when the bridge starts; restart it after a change.
Revoking a device ends its access to services as it does for photos.

## What the bridge does

A phone can only name a catalog `id`. The bridge sends exactly:

```
GET /view.json HTTP/1.1
Host: 127.0.0.1:PORT
Accept: application/json
Connection: close
```

No cookies, credentials or anything from the phone. One 15-second deadline
covers connecting, sending and reading. Only status 200 is accepted; redirects
are not followed. The body may be framed by `Content-Length`, `chunked` or the
connection closing, and reading stops past `max_bytes`. One fetch per service
runs at a time; a request while one runs waits for that fetch's result. A
fetch always ends with an answer: if its worker fails unexpectedly, everyone
waiting gets `unavailable` and the next request starts a new fetch.

Framing is checked to its end. The header block must end within 16 KiB. A
chunk size is compared with what is left under `max_bytes` before anything is
read, so no size can get past the cap. After the last chunk, trailer lines are
read and ignored up to the blank line that ends the body, within 16 KiB.

The body must be JSON and a valid view document (below). The bridge sends on
its normalised form only: known fields, canonical UTC times, links checked,
plus `fetched_at`, the bridge's own clock when it read the document. See
[Freshness](#freshness) for how the page uses it.

The fetch is a small loopback-only client in `crates/homepool-bridge/src/services.rs`.
The S3 adapter's HTTP code was not reused as is: it needs `Content-Length`,
resolves names, speaks TLS, has a 30-second budget and maps S3 errors. The new
client keeps its patterns (one deadline applied to every read and write,
`httparse`, `Connection: close`) and adds `chunked` and close-delimited bodies,
which common servers use for larger responses. No new crate is added;
`httparse` was already in the lockfile.

## Protocol

Four records are added at the end of the closed schema
([protocol](off-lan-protocol.md)); existing type numbers are unchanged.

| Type | Fields |
| --- | --- |
| `services` (20) | `id` |
| `services-list` (21) | `id`, `services`: up to 16 × (`id`, `title`) |
| `service-get` (22) | `id`, `service` |
| `refused` (23) | `id`, `reason`: `unknown`, `not-allowed`, `unavailable`, `too-large` or `invalid` |

`services()` returns only the services the device's grant allows.
`service-get` answers with `get-head` (MIME `application/json`), chunks and
`get-end`, with the same credits and ACKs as `get`, or with `refused`.
`unknown` is an ID not in the catalog; `not-allowed`, a device without a grant;
`unavailable`, a service that is down, slow, redirects or answers with another
status; `too-large`, a body over `max_bytes` or a normalised document over
1 MiB + 1 KiB, the most the page takes; `invalid`, a body that is not a
valid view document. `protocol/service-vectors.json` holds exact bytes and
refusals that both codecs must agree on.

## View document, version 1

```json
{
  "version": 1,
  "title": "Work status",
  "as_of": "2026-01-01T00:00:00Z",
  "valid_until": "2026-01-01T00:10:00Z",
  "refresh_s": 60,
  "state": "warn",
  "notes": ["Nightly jobs finished at 02:10."],
  "sections": [
    {
      "title": "Deploys",
      "state": "stuck",
      "summary": "One waiting",
      "items": [
        { "title": "Staging", "state": "stuck", "detail": "Waiting for approval",
          "meta": ["build 88"], "link": "https://example.org/deploys/88",
          "updated": "2026-01-01T00:00:00Z", "progress": { "value": 3, "target": 5 } }
      ]
    }
  ]
}
```

The same rules run in `crates/homepool-bridge/src/view.rs` and
`web/remote/view.mjs`, checked against `protocol/view-vectors.json`:

- `version` must be 1; `as_of` and `valid_until` are required and
  `valid_until` is not before `as_of`.
- Times are RFC 3339 with `T` and `Z` or a numeric offset, years 0000–9999;
  fractions are dropped and times become UTC.
- States: `ok`, `warn`, `stuck`, `stale`, `error`, `unknown`. Any other string
  becomes `unknown`. A missing section state is the worst of its items.
- Bounds: 20 sections, 200 items in total, 20 notes, 8 `meta` per item,
  500 characters per string. `refresh_s` is rounded and clamped to 30–3600,
  default 60. Going past a bound, or a wrong JSON type, refuses the document.
- Control characters other than a newline become spaces.
- `link` is kept only when it starts with `https://`, parses, and carries no
  user or password; anything else is dropped. The 500-character bound applies
  to the link as written and again as the URL parser writes it out, so a link
  that percent-encoding grows past 500 refuses the document on both sides.
  Normalising a normalised document changes nothing, in either language. `progress` needs numbers with
  `value` ≥ 0 and `target` > 0.
- Unknown fields are ignored.

The notes bound (20) and the 16-service catalog bound are additions; #59 did
not bound either.

## On the page

Tabs appear after Photos only when `services()` returns a non-empty list.
The last tab used is remembered, so a page opened for a status check lands on
it. The photo list is fetched only when Photos is shown.

- **When it fetches:** when a tab opens, once when the page becomes visible
  again on that tab, and every `refresh_s` while visible. Nothing is fetched
  while hidden and nothing pings while idle ([#57's rules](remote-page.md#connection)).
  Refreshes reuse the open session, so a visible tab costs no new relay
  upgrades, but each refresh still uses relay duration ([Cost](#cost)).
- **What comes first:** a header with the overall state, the age of `as_of`
  and counts per state; then sections with the worst state first, and within
  each, items worst first. A section's badge shows the worse of its own state
  and its items'. All-OK sections are folded when something else needs
  attention. The tab itself gets a coloured mark, with words for screen
  readers, when its service needs attention or is out of date.
- **Stale:** past `valid_until` ([Freshness](#freshness)), the page says
  "Out of date" and when the data was good until, the header reads
  "Snapshot from 14:02 · Expired 3 min ago" rather than an update time, and
  every badge turns grey: OK reads "Was OK". This is re-checked every
  10 seconds without a fetch.
- **Text only:** every string goes into `textContent`. Links open in a new
  tab with `rel="noopener noreferrer"` and show their host name.
- **Failures** are said in plain words with a retry, and the last document
  stays visible underneath with its age. A service-side problem keeps the
  normal schedule; a connection problem waits for the next action or return.

### Freshness

The page judges staleness by home's clock: `fetched_at` plus the time since
the page asked for the document. Counting starts when the request is sent, not
when the document arrives, so a slow delivery counts against it. That time is
the larger of two phone clocks: the steady clock (`performance.now()`), which
setting the clock does not move but which may pause while the phone sleeps,
and the wall clock, which keeps counting through sleep. The count never goes
backwards, so a document that has gone stale stays stale.

So a slow delivery, a sleeping phone, or a phone clock that is wrong or is
changed after receipt cannot make old data look fresh; at worst a clock moved
forward makes it look out of date early. A refresh starts the count again from
the new document.

What this does not fix is a wrong clock at home. The service's times and
`fetched_at` both come from the home computer, so they agree with each other
if that clock is off, but a service that copies times from another machine
needs both clocks right. If `fetched_at` were missing, the page would fall back
to its own wall clock at the request; the bridge always adds it.

### Cost

A visible tab refreshes every `refresh_s` on the open session, and each refresh
is a short burst of relay messages. The relay's conservative accounting
([relay](../relay/README.md)) charges each burst after an idle gap up to 31
seconds of handler duration. So a continuously visible tab at `refresh_s` 60
uses about 1,860 seconds an hour, and uses the route's daily duration guard
(16,000 seconds with one route) in about 8.6 visible hours. It is sooner with
other activity or more routes, and about twice as fast at `refresh_s` 30. When
the guard is spent, the relay refuses work until UTC midnight. Tabs suit a
status check that opens, looks and closes, which is one or two bursts. They do
not suit a dashboard left on screen all day. These figures are estimates from
the relay's budget, not measured billing.

### Failures

| Reason | The page says |
| --- | --- |
| `unavailable` | Status isn't answering at home right now. Check that it's running on the home computer, then try again. |
| `too-large` | Status sent more than its size limit, so home didn't pass it on… |
| `invalid` | Status sent something this page can't read… |
| `not-allowed` | This phone isn't allowed to see Status. To allow it, add this phone to Status in the bridge settings at home. |
| `unknown` | Status isn't set up at home anymore. Reload this page to update the list. |

## Tests

- `cargo test -p homepool-bridge`: catalog URL and field checks; every fetch
  refusal (redirect, other statuses, oversize by length, chunked and close,
  invalid JSON, invalid document, down, silent and dripping services past one
  deadline); chunk sizes up to `u64::MAX` against the cap, a missing or endless
  final trailer, and a header block exactly at and one byte over its cap; one
  fetch shared by concurrent requests, and waiters that still get an answer
  when the worker panics; and through a paired
  session, the grant-filtered list, `unknown`, `not-allowed`, `unavailable`,
  a full document over credits, and a revoked device.
- `node --test web/protocol/tests/*.test.mjs`: both codecs on
  `protocol/service-vectors.json`.
- `node --test web/remote/tests/*.test.mjs`: the shared view vectors
  (including links that percent-encoding grows to and past the bound, and
  normalising twice), hostile strings, links, staleness with a wrong phone
  clock, a slow delivery, the clock set back or forward after receipt, a
  sleeping phone, ordering, and the client operations against the scripted
  bridge (counting from the request, not the arrival).
- `web/remote/tests/services-e2e.mjs`: the explicit browser runner, with the
  same environment as `e2e.mjs` ([remote page](remote-page.md#tests)). It adds
  a loopback fixture service and covers the empty catalog, tabs, a healthy
  document, stuck and warn items first, a document going stale by the clock
  ("Snapshot from … · Expired …"),
  the service stopping and coming back, oversize, invalid, redirecting and
  hostile documents, hide and return (no fetch while hidden, one on return,
  then one per `refresh_s` on the same session), a phone without a grant,
  WebKit, the remembered tab, and revocation.

Unproven: physical phones (iOS Safari, Linux-phone Firefox), real WSS through
Cloudflare, and real services other than the fixture.
