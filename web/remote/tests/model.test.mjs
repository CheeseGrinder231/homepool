import assert from "node:assert/strict";
import { test } from "node:test";
import {
  BlobCache,
  CancelledError,
  LinkError,
  WorkQueue,
  fingerprint,
  imageType,
  kindLabel,
  newestFirst,
  parsePairing,
  uploadName,
  uploadType,
} from "../model.mjs";

const origin = ["https", "//192.0.2.1"].join(":");
const fields = {
  route: "example",
  token: "ab".repeat(32),
  bridge: "cd".repeat(32),
  invitation: "ef".repeat(16),
  psk: "01".repeat(32),
};
const fragment = (f = fields) =>
  Object.entries(f)
    .map(([k, v]) => `${k}=${v}`)
    .join("&");

test("pairing links: the exact five fields, once each", () => {
  assert.deepEqual(parsePairing(`${origin}/#${fragment()}`, origin), fields);
  assert.deepEqual(parsePairing(`#${fragment()}`, origin), fields);
  assert.deepEqual(parsePairing(fragment(), origin), fields);
  assert.deepEqual(parsePairing(`  ${origin}/#${fragment()}\n`, origin), fields);
});

test("pairing links: hostile and broken links are refused", () => {
  const refused = (text, reason = "malformed") =>
    assert.throws(
      () => parsePairing(text, origin),
      (e) => e instanceof LinkError && e.reason === reason,
      text,
    );
  refused("");
  refused(`${origin}/`);
  refused(`${origin}/#`);
  refused(`${origin.replace(".1", ".9")}/#${fragment()}`, "other-origin");
  refused(`${origin}/other#${fragment()}`);
  refused(`${origin}/?x=1#${fragment()}`);
  refused(`javascript:alert(1)//${fragment()}`);
  refused(`${fragment()}&x=<script>alert(1)</script>`);
  refused(`${fragment()}&route=other`);
  refused(`${fragment()}&`);
  refused(fragment({ ...fields, token: "AB".repeat(32) }));
  refused(fragment({ ...fields, token: "ab".repeat(31) }));
  refused(fragment({ ...fields, bridge: "zz".repeat(32) }));
  refused(fragment({ ...fields, invitation: "ef".repeat(32) }));
  refused(fragment({ ...fields, route: "a/b" }));
  refused(fragment({ ...fields, route: "%2e%2e" }));
  refused(fragment({ ...fields, route: "r".repeat(65) }));
  refused(`${fragment()}&psk=${fields.psk}`);
  const { psk, ...missing } = fields;
  void psk;
  refused(fragment(missing));
  refused(`#${fragment()}`.padEnd(1100, "a"));
});

test("upload names and types fit the protocol fields", () => {
  assert.equal(uploadName("IMG_0001.JPG"), "IMG_0001.JPG");
  assert.equal(uploadName("\u0000\u001f\u007f"), "photo");
  assert.equal(uploadName("a\nb\tc.jpg"), "abc.jpg");
  const long = uploadName("é".repeat(300));
  assert.equal(new TextEncoder().encode(long).length, 254);
  assert.equal(uploadName("😀".repeat(80)).length, 63 * 2);
  assert.equal(uploadType("image/jpeg"), "image/jpeg");
  assert.equal(uploadType("Image/HEIC"), "image/heic");
  assert.equal(uploadType(""), "");
  assert.equal(uploadType("text/html; charset=utf-8"), "");
  assert.equal(uploadType("x".repeat(200) + "/y"), "");
});

test("only fixed raster types ever label a Blob for display", () => {
  assert.equal(imageType("2026-11-02/x-photo.jpg", "image/jpeg"), "image/jpeg");
  assert.equal(imageType("2026-11-02/x-photo.jpg", ""), "image/jpeg");
  assert.equal(imageType("2026-11-02/x-photo.HEIC", ""), "image/heic");
  assert.equal(imageType("2026-11-02/x-photo.jpg", "application/octet-stream"), "image/jpeg");
  for (const [key, mime] of [
    ["2026-11-02/x-a.svg", "image/svg+xml"],
    ["2026-11-02/x-a.svg", ""],
    ["2026-11-02/x-a.jpg", "image/svg+xml"],
    ["2026-11-02/x-a.jpg", "text/html"],
    ["2026-11-02/x-a.html", ""],
    ["2026-11-02/x-a", ""],
  ])
    assert.equal(imageType(key, mime), null, `${key} ${mime}`);
  assert.equal(kindLabel("2026-11-02/x-a.heic", ""), "heic");
  assert.equal(kindLabel("2026-11-02/x-a", "image/avif"), "avif");
  assert.equal(kindLabel("2026-11-02/x-a", "text/<b>"), "file");
});

test("fingerprints group the public key and order keeps newest first", () => {
  assert.equal(fingerprint(new Uint8Array([0, 1, 0xab, 0xcd, 0xef, 0x10])), "0001 abcd ef10");
  const keys = ["2026-11-01/120000-a", "2026-11-03/080000-b", "2026-11-02/235959-c"];
  assert.deepEqual(
    newestFirst(keys.map((key) => ({ key }))).map((o) => o.key),
    ["2026-11-03/080000-b", "2026-11-02/235959-c", "2026-11-01/120000-a"],
  );
});

test("the Blob URL cache is bounded, revokes on eviction and keeps pins", () => {
  const live = new Set();
  const evicted = [];
  let n = 0;
  const cache = new BlobCache({
    entries: 3,
    bytes: 100,
    create: () => {
      const url = `blob:${n++}`;
      live.add(url);
      return url;
    },
    revoke: (url) => live.delete(url),
    onEvict: (key) => evicted.push(key),
  });
  const blob = (size) => ({ size });
  cache.put("a", blob(10));
  cache.put("b", blob(10));
  cache.pin("a");
  cache.put("c", blob(10));
  cache.put("d", blob(10));
  assert.deepEqual(evicted, ["b"], "least recent unpinned goes first");
  cache.get("c");
  const big = cache.put("e", blob(95));
  assert.deepEqual(evicted, ["b", "d", "c"], "bytes bound, pinned entry kept");
  assert.ok(live.has(big.url), "the entry just added is never revoked");
  assert.ok(cache.map.has("a"));
  cache.unpin("a");
  assert.deepEqual(evicted, ["b", "d", "c", "a"]);
  assert.equal(live.size, 1);
  cache.put("e", blob(5));
  assert.equal(live.size, 1, "replacing a key revokes the old URL");
  cache.clear();
  assert.equal(live.size, 0);
  assert.equal(cache.size, 0);
});

test("one queue: priority order, cancel queued work, promote, pause", async () => {
  const ran = [];
  let release;
  const gate = new Promise((r) => (release = r));
  const queue = new WorkQueue(async (work) => {
    ran.push(work);
    if (work === "first") await gate;
    return work;
  });
  const first = queue.add(1, "first");
  const tile1 = queue.add(3, "tile1");
  const tile2 = queue.add(3, "tile2");
  const upload = queue.add(2, "upload");
  const preview = queue.add(0, "preview");
  assert.equal(first.started, true);
  assert.equal(tile1.cancel(), true);
  await assert.rejects(tile1.done, CancelledError);
  tile2.promote(0);
  assert.equal(first.cancel(), false, "running work cannot be cancelled");
  release();
  assert.equal(await first.done, "first");
  await Promise.all([tile2.done, upload.done, preview.done]);
  assert.deepEqual(ran, ["first", "tile2", "preview", "upload"]);
  queue.pause();
  const later = queue.add(1, "later");
  await new Promise((r) => setTimeout(r, 10));
  assert.equal(later.started, undefined);
  queue.resume();
  assert.equal(await later.done, "later");
  const a = queue.add(1, "x");
  const b = queue.add(1, "y");
  queue.clear();
  await a.done;
  await assert.rejects(b.done, CancelledError);
});
