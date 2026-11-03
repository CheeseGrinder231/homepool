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
runs at a time; a request while one runs waits for that fetch's result.

The body must be JSON and a valid view document (below). The bridge sends on
its normalised form only: known fields, canonical UTC times, links checked,
plus `fetched_at`, the bridge's own clock when it read the document. The page
uses that clock to judge staleness, so a phone whose clock is wrong cannot make
old data look fresh.

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
status; `too-large`, a body over `max_bytes`; `invalid`, a body that is not a
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
  user or password; anything else is dropped. `progress` needs numbers with
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
  upgrades.
- **What comes first:** a header with the overall state, the age of `as_of`
  and counts per state; then sections with the worst state first, and within
  each, items worst first. A section's badge shows the worse of its own state
  and its items'. All-OK sections are folded when something else needs
  attention. The tab itself gets a coloured mark, with words for screen
  readers, when its service needs attention or is out of date.
- **Stale:** past `valid_until` by home's clock, the page says "Out of date",
  when the data was good until, and every badge turns grey: OK reads
  "Was OK". This is re-checked every 10 seconds without a fetch.
- **Text only:** every string goes into `textContent`. Links open in a new
  tab with `rel="noopener noreferrer"` and show their host name.
- **Failures** are said in plain words with a retry, and the last document
  stays visible underneath with its age. A service-side problem keeps the
  normal schedule; a connection problem waits for the next action or return.

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
  deadline); one fetch shared by concurrent requests; and through a paired
  session, the grant-filtered list, `unknown`, `not-allowed`, `unavailable`,
  a full document over credits, and a revoked device.
- `node --test web/protocol/tests/*.test.mjs`: both codecs on
  `protocol/service-vectors.json`.
- `node --test web/remote/tests/*.test.mjs`: the shared view vectors, hostile
  strings, links, staleness with a wrong phone clock, ordering, and the client
  operations against the scripted bridge.
- `web/remote/tests/services-e2e.mjs`: the explicit browser runner, with the
  same environment as `e2e.mjs` ([remote page](remote-page.md#tests)). It adds
  a loopback fixture service and covers the empty catalog, tabs, a healthy
  document, stuck and warn items first, a document going stale by the clock,
  the service stopping and coming back, oversize, invalid, redirecting and
  hostile documents, hide and return (no fetch while hidden, one on return,
  then one per `refresh_s` on the same session), a phone without a grant,
  WebKit, the remembered tab, and revocation.

Unproven: physical phones (iOS Safari, Linux-phone Firefox), real WSS through
Cloudflare, and real services other than the fixture.
