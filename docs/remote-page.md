# Remote image page

Slice 4 of [#50](https://github.com/CheeseGrinder231/homepool/issues/50).
`web/remote` is the image page for use away from home: on home Wi-Fi or mobile
data, a paired phone adds, lists, previews and downloads photos through the
[relay](../relay/README.md) and the [home bridge](bridge.md). Everything between
the phone and home is end-to-end encrypted; the relay sees ciphertext only.

Like the [LAN page](images.md) it is plain HTML, CSS and JavaScript modules: no
build step, no dependencies, no framework and no service worker. The Noise
protocol and codecs come from `web/protocol`; it shares `web/images/style.css`
and `web/images/model.mjs` with the LAN page, which is unchanged.

## Served by the relay

The relay Worker serves the page from its static assets (`web/`), on the same
origin as the relay WebSocket. `relay/wrangler.toml` runs the Worker first for
every request, and the Worker serves exactly the files listed in `PAGE` in
`relay/worker.mjs`, at `/` for the page itself. Tests, the LAN page's own code
and any other path get 404; `web/.assetsignore` keeps tests out of the upload.
Page responses carry:

- `Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' blob:; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`;
  `connect-src 'self'` covers the same-origin `wss:` relay socket;
- `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`,
  `Cross-Origin-Opener-Policy` and `-Resource-Policy: same-origin`, a
  `Permissions-Policy` denying camera, microphone, location and payment,
  `X-Robots-Tag: noindex`, and `Cache-Control: no-cache` with the asset ETag.

The kill switch stops the page too. Page files cost Worker requests (about a
dozen per page load, fewer when cached); they wake no Durable Object and write
nothing. The bridge's `page_origin` is this relay origin with path `/`.

## Pairing

At home, `homepool-bridge invite CONFIG` prints a link; show it as a QR code.
The fragment carries the route, the invitation token, the bridge public key,
the invitation ID and the PSK, and never reaches a server.

On the phone:

1. Scan the code with the camera and open the link. A phone that cannot scan
   can paste the link under "No camera? Paste the link instead".
2. The page reads the fragment once and removes it from the address bar and
   the history entry, including when the page was already open.
3. It shows "Pairing this phone…" with three steps, then **Paired**, with a
   button to the photos.
4. Under "Check this phone at home (optional)" it shows the phone's key in
   groups of four. `homepool-bridge devices CONFIG` shows the same value; check
   it there if a pairing looks unexpected.

A link is refused with a plain message, and any existing pairing kept, when it
is not exactly the five fields in the expected form, or comes from another
address. Every other refusal is learned from home: an expired or already used
code, or one carrying the wrong home key, all read "This pairing code didn't
work… Show a new code at home and scan it again." The invitation is not used
up by a failed attempt. A phone that is already paired asks before pairing
again, and keeps its current pairing unless the new one completes.

The page never shows protocol words, hex (outside the optional check) or raw
errors. Each failure says what happened and what to do.

## What the phone stores

IndexedDB `homepool-remote` holds:

- a non-extractable AES-GCM key;
- the X25519 private key as `wrapKey("pkcs8")` output with its IV, plus the
  raw public key. Safari reads a stored X25519 `CryptoKey` back as null
  (WebKit bug 312279), so the key is generated extractable only long enough to
  wrap it, and every load unwraps a non-extractable copy;
- the route and bridge public key, and the device ID and device token from the
  encrypted enrollment acknowledgement;
- during pairing only, the invitation token, until the acknowledgement arrives;
- the IDs, names and sizes of uploads that began but are not settled.
  These stay until home reports an outcome or the owner dismisses them, never
  dropped for age or count. While 50 are unsettled, a new upload is refused
  before anything is sent ("Not sent… check or dismiss them"), so every
  attempt home may hold can still be checked.

Once pairing completes, and on Forget, the page drops the link it was opened
with and empties the paste box. The PSK is never stored or sent. Same-origin page code could unwrap an
extractable copy of the key; that is inside the accepted page-code trust
boundary of #50. The page asks for persistent storage; if the browser clears
it anyway, the phone pairs again.

If the enrollment acknowledgement is lost after the final handshake message,
the page reconnects once on the invitation token with the same key, as the
bridge allows, and receives its device token then. If that fails, it says
pairing did not finish. There is no fallback from reconnect to enrollment.

**Forget this phone** closes the session, cancels queued work, revokes every
Blob URL and deletes the database. It is local cleanup: revoke the device at
home as well (`homepool-bridge revoke CONFIG ID`).

## Connection

One relay session and one Noise session per phone, opened on demand: when the
library opens, on a user action, for queued work, or once when the library
becomes visible again. There are no idle pings. An idle session is closed
after three minutes, and at once when the page is hidden, which frees the
relay's session slot. Each connection is one of the
relay's 1,000 daily upgrades, shared with the bridge's reconnects, and one or
two bursts of its daily duration budget ([cost](../relay/README.md)). When the page becomes visible
again during a transfer, the page sends one encrypted ping with a 5-second
deadline; a dead session interrupts the transfer, which then recovers below.
While an operation is outstanding, the page pings every 15 seconds with a
10-second pong deadline. Pings never count as progress.

Deadlines: 10 s to open and admit, 10 s for the handshake, 75 s for a reply
from home's storage, and 30 s without file bytes or credits during a transfer.

The relay closes a refused phone admission with a code the page explains:

| Code | Meaning | The page says |
| --- | --- | --- |
| 4002 | Home's bridge is not connected | Your pool at home isn't connected right now |
| 4001 | Token unknown or expired | For a pairing code: it didn't work. For a paired phone: looked at again after 8 s, then "removed at home" |
| 4003 | All session slots in use | Too many devices are connected right now |

A socket that never opens reads "Can't reach the relay". The second look on
4001 covers the seconds after the bridge reconnects, when it re-registers
tokens. A paired phone whose key home no longer accepts reads "Home didn't
recognize this phone". In both cases the page shows the pairing screen with
"Check again"; its records stay until it pairs again or is forgotten. After a
connection failure, queued work waits for the next action, "Try again" or the
page becoming visible, rather than dialling again on its own. A return that
cannot connect says why once and does not retry until the next return or
action.

## Transfers

Flow control: downloads grant eight chunks, then top up four at a time, so at
most eight 60 KiB chunks (492,032 envelope bytes) are ever outstanding, under
the 512 KiB end-to-end window. Uploads send only against the bridge's credits
and wait while the browser buffers more than 1 MiB. The page ACKs the relay
every 64 KiB received, after each handshake message and each non-chunk record,
before granting credit and before closing.

Uploads read one `file.slice()` per chunk, never the whole file. Each file gets
a fresh random request ID, kept with that `File` and never reused for another.
Files over 50 MiB, the bridge's limit, are refused before sending. Just before
`put-begin` leaves the page, the ID, name and size are stored; from then on the
outcome comes from home only. An upload without `put-done` asks `put-status`:

- `committed`: added;
- `failed`: "Upload failed", with **Upload again** under a new ID;
- `pending`: "Not confirmed yet", with **Check again**;
- `unknown`: "Not confirmed. Home has no record of it", pointing to the list;
- no answer: "Not confirmed" with the reason and **Check again**.

A fresh upload is offered only when the attempt failed: `failed`, an explicit
refusal of `put-begin`, or a failure before `put-begin` was sent. A reload, or
iOS discarding the tab in the background, keeps the stored IDs, and the page
checks them when it opens again. While anything transfers, the page says to
keep it open.

Downloads are assembled into a Blob only after a matching end record. Grid
tiles, the preview and the download button share one queue per phone. The
preview and download go first, then listing, uploads and tiles. A tile that
scrolls away before its turn is dropped from the queue; work that has started
runs to its end, because cancelling it would cost a new session. Images over
16 MiB show a file tile until tapped. Decoded files go into a Blob URL cache of
40 entries and 96 MiB that revokes URLs on eviction and never evicts the open
preview. A tile still on screen keeps its picture and reloads only after it
has scrolled away and back.

## Untrusted files

Names, keys and MIME types from home are text only. A Blob URL is same-origin,
so the page labels Blobs only with a fixed raster type (JPEG, PNG, GIF, WebP,
AVIF, HEIC, HEIF, BMP) or `application/octet-stream`, never the stored type.
SVG, HTML and anything else render as a file tile. Downloads always use an
`application/octet-stream` Blob, so a stored HTML or SVG file never becomes a
document in the page's origin. Files the browser cannot decode, such as HEIC
in Firefox and Chromium, still get a tile with their type and size and a
working download. The "one copy, not a backup" notice stays next to **Add
photos**.

## Tests

```sh
node --test web/remote/tests/*.test.mjs relay/tests/*.test.mjs
```

`model.test.mjs` covers link parsing (including hostile links), name and type
handling, the Blob URL cache, the queue and the unsettled-upload record
(nothing dropped, concurrent changes kept, the bound). `client.test.mjs` runs the page's
connection code with real Noise against a scripted relay and bridge: pairing
with the wrapped key, credits and the relay window, interrupted uploads and
`put-status`, close codes, refused codes keeping a pairing, a lost enrollment
acknowledgement, Forget, and the single reconnect on visibility return. `relay/tests/page.test.mjs` covers page serving,
headers and refusals, and the admission close codes.

`web/remote/tests/e2e.mjs` is an explicit runner with real browsers at phone
size (390×844) against a throwaway Garage, the relay in local workerd serving
the page from `web/`, and the bridge. Install Wrangler/Miniflare in scratch as
in [the relay guide](../relay/README.md#reproduce-locally), build the bridge,
and point it at Playwright:

```sh
cargo build -p homepool-bridge --locked
HOMEPOOL_TEST_GARAGE=/path/to/garage \
MINIFLARE_MODULE="$TMPDIR/relay-tools/node_modules/miniflare/dist/src/index.js" \
PLAYWRIGHT_MODULE=/path/to/node_modules/playwright/index.mjs \
SCREENSHOTS="$TMPDIR/remote-shots" \
  node web/remote/tests/e2e.mjs
```

`CARGO_TARGET_DIR` must point at the build; `BROWSERS` defaults to
`chromium,firefox,webkit` and `CHROMIUM_PATH` selects a system Chromium. It
pairs from the link, uploads a 12 MB image among others, and lists, previews
and downloads it with a matching SHA-256. It reloads and stays paired through
`unwrapKey`, and runs two browsers as two phones at once, the second paired
from a pasted link that is cleared afterwards. Each phone is hidden and shown
again: the idle session closes, and exactly one new session opens without a
tap. It kills the relay
and then the bridge mid-upload and checks that the page shows the journal's
outcome, followed by a fresh upload that commits. It also covers a store write
confirmed before the journal commit. It then refuses a malformed link, a
hostile link, a wrong bridge key, a used invitation and an expired invitation,
each keeping the pairing, and pairs WebKit with the invitation the wrong-key
attempt left unused. Finally it forgets one phone and revokes another at home,
then checks the "removed at home" screen. No live configuration, credentials,
Garage, Caddy or Cloudflare account is used.

Unproven: physical iPhone Safari and Linux-phone Firefox (camera scanning,
touch, backgrounding and tab discarding, storage eviction, memory with many
large tiles), real WSS through Cloudflare and its close-code delivery, and
50 MiB transfers over mobile data.
