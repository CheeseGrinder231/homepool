# Image page

`web/images` is a static page for phones: add photos to one Garage bucket,
see them in a grid, preview and download them. It is plain HTML, CSS and
JavaScript modules with no build step, no dependencies, no service worker and
no backend. The browser signs S3 requests itself with WebCrypto and talks to
the bucket on the same origin through Caddy, so there is no CORS.

**One copy, not a backup.** The page says so next to the upload button. The
pool keeps a single copy of each photo; keep the originals on the phone.

Out of scope on purpose: background sync, delete, thumbnails, multipart
uploads, accounts, jobs and the Homepool API. The grid shows the originals,
loaded lazily as tiles scroll into view.

## Serve it with Caddy

[`docs/examples/images.Caddyfile`](examples/images.Caddyfile) is the site.
It takes four values from Caddy's environment, or edit them in place:

| Variable | Meaning |
| --- | --- |
| `HOMEPOOL_IMAGES_ADDRESS` | The address phones open, for example `pool.example.net:8443`. |
| `HOMEPOOL_IMAGES_ROOT` | The checked-out `web/images` directory. |
| `HOMEPOOL_IMAGES_BUCKET` | The consumer bucket, for example `homepool-consumer-1`. |
| `HOMEPOOL_GARAGE_S3` | Garage's S3 API address, for example `localhost:3900`. |

The site uses `tls internal`. Phones must trust Caddy's local root certificate
first, because WebCrypto only runs on a secure origin. Keep Garage's S3 port
bound to loopback so that Caddy is the only way in from the network.

What the site enforces, and what `web/images/tests/site.test.mjs` checks:

- Only the three request shapes the page sends reach Garage, on that one
  bucket: the exact `ListObjectsV2` query with the page's fixed page size and
  Garage's continuation token, a presigned `GET` of one object with exactly the
  page's query parameters, and a `PUT` of one object under a key of the shape
  the page creates (`YYYY-MM-DD/HHMMSS-<26 base32 characters>-<safe name>`),
  with no query and no `x-amz-copy-source`. Everything else is refused before
  Garage sees it: the bucket listing at `/`, other buckets, dot segments,
  double slashes and percent-encoded paths, `HEAD`, `DELETE`, `POST`, multipart,
  tagging, ACLs, copies, and bucket configuration such as lifecycle rules. The
  test proves the last two with a lifecycle body and a copy that Garage accepts
  from the same key when asked directly.
- Through Caddy, a leaked key can therefore list, read, add and overwrite
  objects. It cannot delete them, expire them through a lifecycle rule, copy
  them or change the bucket. Overwriting is possible because Garage 2.1.0 has
  no create-only `PUT`; see the next section. Anything the key may do in Garage
  remains possible for whoever reaches Garage's S3 port directly, so keep it on
  loopback. Bucket names with a dot would need escaping in the regular
  expressions.
- Objects come back with `Content-Disposition: attachment`, a
  `Content-Security-Policy: sandbox` and `nosniff`. A stored HTML or SVG file
  opened directly downloads instead of rendering, and even where a browser
  renders it, it runs in an opaque origin with no script and no access to the
  page's storage. A stored `Content-Disposition` cannot override this.
- The page itself gets a strict CSP (`script-src 'self'`, no inline code) and
  never renders strings from the network as markup, so a hostile object key is
  shown as text. Keys stored by other clients that fall outside the allowed shape
  are listed but cannot be fetched through Caddy; the page shows a file tile.
- Garage's own checks still apply: unsigned, wrongly signed and expired
  requests are refused.

Caddy writes no access log unless configured. If you enable one, presigned
query strings land in it; they expire within minutes and do not contain the
secret, but treat the log as private.

## Keys are unique by randomness

Each upload gets the key `YYYY-MM-DD/HHMMSS-<tag>-<safe name>`, where the tag
is 16 bytes from `crypto.getRandomValues` in base32. Garage 2.1.0 offers no
create-only write: its `PutObject` handler reads no `If-None-Match` or other
precondition header, so there is nothing a client can ask for that makes a
`PUT` fail when the key already exists, and a check before the `PUT` would not
be atomic either. The page therefore checks nothing and relies on the 128
random bits; two phones producing the same tag in the same second is not a
realistic event. Should a later Garage add a conditional `PUT`, the page should
send it and the site should require it.

## Give the key to a phone

`homepool create-consumer` writes a credentials file; see
[the Garage guide](garage.md#credentials-file-format-version-1). The page wants
the key ID, the secret, the bucket and the Garage `s3_region` in the URL
fragment of a setup link. The fragment never reaches a server. Show it as a QR
code in the terminal; do not write it to a file:

```sh
python3 - "$CREDENTIALS_FILE" "https://pool.example.net:8443" "garage" <<'EOF' | qrencode -t ANSIUTF8
import json
import sys
import urllib.parse

with open(sys.argv[1], encoding="utf-8") as source:
    credentials = json.load(source)
if credentials["version"] != 1:
    raise SystemExit("unsupported credentials file version")
fields = {
    "k": credentials["aws_access_key_id"],
    "s": credentials["aws_secret_access_key"],
    "b": credentials["bucket"],
    "r": sys.argv[3],
}
print(f"{sys.argv[2]}/#{urllib.parse.urlencode(fields, quote_via=urllib.parse.quote)}")
EOF
```

Replace the address with the one in `HOMEPOOL_IMAGES_ADDRESS` and the region
with your Garage `s3_region`. Drop the `| qrencode …` part to print the link
instead, for the paste field on a phone that cannot scan.

On the phone:

1. iPhone: point the Camera at the code and open the link in Safari. Firefox:
   scan from the address bar, or paste the link into the page's setup field.
2. The page removes the fragment from the address bar, lists the bucket with
   the key and only then keeps it in the browser's storage. Reloads stay
   connected. A link opened on a phone that already has a key is not applied
   silently: the page asks whether to replace the current key, and keeps it
   if the new one cannot list the bucket.
3. Add the page to the home screen if you like; it opens in the browser and
   shares the same storage.

**The setup link is the key.** Anyone who scans it can read and write the
bucket. Scanned links may linger in browser history, in synced history on other
devices and in a QR app's scan history; a pasted link sits in the clipboard,
which some phones sync between devices. Use the QR path, tap "Forget this phone"
on a phone you hand over, and revoke a leaked key in Garage (`garage key delete`)
before creating a new consumer. Safari removes a site's storage after seven days
of use without visiting it, so keep the credentials file to show the code again.

## What phones do with HEIC

Safari on iOS hands the page a JPEG when you pick a HEIC photo from the
library, whatever the `accept` attribute says, so uploads from an iPhone are
JPEG. Safari 17 and later also render HEIC in `<img>`. Firefox does not decode
HEIC on any platform. Files a browser cannot render still get a tile with the
file type and a working download in the preview.

## Presigned URLs

Grid tiles and previews use presigned GET URLs valid for ten minutes;
the download link is signed for five minutes at the moment it is tapped, so a
preview left open does not go stale. Whoever holds such a URL can fetch that
one object until it expires; it does not reveal the secret. The URLs are only
ever placed in `<img>` sources and the download link, never in the address bar.
Garage judges expiry by its own clock against the signing time, and allows the
usual skew, so a phone with a wrong clock sees shorter or longer windows.

## Tests

```sh
node --test web/images/tests/sigv4.test.mjs web/images/tests/model.test.mjs
HOMEPOOL_TEST_GARAGE=/path/to/garage HOMEPOOL_TEST_CADDY=/path/to/caddy \
  node --test web/images/tests/site.test.mjs
```

The first command needs only Node. It checks the signer against the AWS
Signature Version 4 test vectors in `web/images/tests/vectors.json` and the pure
helpers, including hostile listing keys. The second starts a throwaway Garage
and a throwaway Caddy with the example site on loopback ports under a
temporary directory and checks the refusals above; it is skipped without the
two variables. Neither touches an existing installation.
