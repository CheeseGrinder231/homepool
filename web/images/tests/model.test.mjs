import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import {
  RANDOM_TAG_BYTES,
  createConnection,
  decodeXml,
  displayName,
  formatBytes,
  looksLikeImage,
  objectKey,
  parseListing,
  parseSetup,
  randomTag,
  safeName,
  setupFragment,
  sortNewestFirst,
} from "../model.mjs";

const config = {
  accessKeyId: "GK0123456789abcdef0123456789",
  secretAccessKey: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  bucket: "homepool-consumer-1",
  region: "garage",
};

test("setup links round-trip through the fragment", () => {
  const fragment = setupFragment(config);
  assert.deepEqual(parseSetup(`#${fragment}`), config);
  assert.deepEqual(parseSetup(`example://pool.example.net:8443/#${fragment}`), config);
  assert.deepEqual(parseSetup(`  ${fragment}\n`), config);
});

test("setup links with missing or malformed fields are refused with a plain message", () => {
  assert.throws(() => parseSetup(""), /incomplete: k is missing/);
  assert.throws(() => parseSetup("k=a&s=b&b=c"), /incomplete: r is missing/);
  assert.throws(() => parseSetup("not a link"), /not a Homepool setup link/);
  assert.throws(() => parseSetup("k=%E0%A4%A&s=b&b=c&r=d"), /not a Homepool setup link/);
  assert.throws(() => parseSetup(setupFragment({ ...config, bucket: "../other" })), /invalid bucket/);
  assert.throws(() => parseSetup(setupFragment({ ...config, region: "ga rage" })), /invalid region/);
  assert.throws(() => parseSetup(setupFragment({ ...config, secretAccessKey: "short" })), /invalid secretAccessKey/);
});

test("file names become safe key segments", () => {
  assert.equal(safeName("IMG_0042.JPG"), "IMG_0042.jpg");
  assert.equal(safeName("holiday photo (1).jpeg"), "holiday-photo-1.jpeg");
  assert.equal(safeName("../../etc/passwd"), "passwd");
  assert.equal(safeName("..hidden"), "image.hidden");
  assert.equal(safeName("<img src=x onerror=alert(1)>.png"), "img-src-x-onerror-alert-1.png");
  assert.equal(safeName("照片.heic"), "image.heic");
  assert.equal(safeName(""), "image");
  assert.equal(safeName("a".repeat(100) + ".png"), `${"a".repeat(60)}.png`);
  assert.match(safeName("x.y.z"), /^x\.y\.z$/);
});

test("random tags carry all 128 bits as 26 base32 characters", () => {
  assert.equal(RANDOM_TAG_BYTES * 8, 128);
  // RFC 4648 vector, lowercased and unpadded.
  assert.equal(randomTag(new TextEncoder().encode("foobar")), "mzxw6ytboi");
  const zero = randomTag(new Uint8Array(RANDOM_TAG_BYTES));
  assert.equal(zero, "a".repeat(26));
  const ones = randomTag(new Uint8Array(RANDOM_TAG_BYTES).fill(255));
  assert.equal(ones, `${"7".repeat(25)}4`);
  const seen = new Set();
  for (let i = 0; i < 256; i += 1) {
    const bytes = new Uint8Array(RANDOM_TAG_BYTES);
    bytes[15] = i;
    seen.add(randomTag(bytes));
  }
  assert.equal(seen.size, 256, "the last byte changes the tag");
});

test("object keys carry the day, time, the random tag and the safe name", () => {
  const tag = randomTag(new Uint8Array(RANDOM_TAG_BYTES).fill(1));
  const key = objectKey("IMG_0042.HEIC", new Date("2026-11-02T14:30:15.250Z"), tag);
  assert.equal(key, `2026-11-02/143015-${tag}-IMG_0042.heic`);
  assert.equal(displayName(key), `143015-${tag}-IMG_0042.heic`);
  // The exact shape the Caddy site allows for PUT.
  assert.match(key, /^[0-9]{4}-[0-9]{2}-[0-9]{2}\/[0-9]{6}-[a-z2-7]{26}-[A-Za-z0-9_][A-Za-z0-9._-]*$/);
});

test("image tiles are attempted only for image extensions", () => {
  assert.equal(looksLikeImage("2026-11-02/x.jpg"), true);
  assert.equal(looksLikeImage("2026-11-02/x.HEIC"), true);
  assert.equal(looksLikeImage("2026-11-02/notes.txt"), false);
  assert.equal(looksLikeImage("2026-11-02/page.html"), false);
  assert.equal(looksLikeImage("noext"), false);
});

test("sizes are readable", () => {
  assert.equal(formatBytes(512), "512 B");
  assert.equal(formatBytes(1536), "1.5 KiB");
  assert.equal(formatBytes(10 * 1024 * 1024), "10 MiB");
  assert.equal(formatBytes(3 * 1024 ** 3), "3.0 GiB");
});

test("listings are parsed and hostile keys stay plain strings", () => {
  const xml = readFileSync(join(dirname(fileURLToPath(import.meta.url)), "listing.xml"), "utf8");
  const listing = parseListing(xml);
  assert.equal(listing.truncated, true);
  assert.equal(listing.continuationToken, "2026-11-02/143015-k7q2m9x4-IMG_0042.jpg");
  assert.deepEqual(
    listing.objects.map((object) => object.key),
    [
      "2026-11-01/090000-aaaaaaaa-first.jpg",
      "2026-11-02/143015-k7q2m9x4-IMG_0042.jpg",
      '<img src=x onerror=alert(1)>/"quoted" & <b>bold</b>.svg',
      "../escape/ሴ.png",
    ],
  );
  assert.equal(listing.objects[1].size, 10485760);
  assert.equal(listing.objects[1].lastModified, "2026-11-02T14:30:16.000Z");
  assert.equal(decodeXml("&unknown; &#65;&#x42;"), "&unknown; AB");
  assert.throws(() => parseListing("<Error><Code>AccessDenied</Code></Error>"), /not a bucket listing/);
  const dated = sortNewestFirst(listing.objects.filter((object) => object.key.startsWith("2026")));
  assert.deepEqual(
    dated.map((object) => object.key),
    ["2026-11-02/143015-k7q2m9x4-IMG_0042.jpg", "2026-11-01/090000-aaaaaaaa-first.jpg"],
  );
});

test("the page never renders strings as markup", () => {
  const source = readFileSync(join(dirname(fileURLToPath(import.meta.url)), "..", "app.mjs"), "utf8");
  for (const api of ["innerHTML", "outerHTML", "insertAdjacentHTML", "document.write", "eval(", "new Function"]) {
    assert.equal(source.includes(api), false, `app.mjs must not use ${api}`);
  }
});

test("the documented setup-link command produces a link the page accepts", () => {
  const doc = readFileSync(join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..", "docs", "images.md"), "utf8");
  const script = doc.match(/<<'EOF' \| qrencode[^\n]*\n([\s\S]*?)\nEOF\n/)[1];
  const scratch = mkdtempSync(join(tmpdir(), "homepool-images-doc-"));
  try {
    const file = join(scratch, "consumer.json");
    writeFileSync(
      file,
      JSON.stringify({ version: 1, bucket: config.bucket, aws_access_key_id: config.accessKeyId, aws_secret_access_key: config.secretAccessKey }),
    );
    // The documented command takes the real origin; any scheme works here.
    const result = spawnSync("python3", ["-I", "-", file, "example://pool.example.net:8443", config.region], { input: script, encoding: "utf8" });
    if (result.error?.code === "ENOENT") return;
    assert.equal(result.status, 0, result.stderr);
    const link = result.stdout.trim();
    assert.ok(link.startsWith("example://pool.example.net:8443/#k="), link);
    assert.deepEqual(parseSetup(link), config);
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
});

function pendingListing() {
  let release;
  const promise = new Promise((done) => {
    release = done;
  });
  return { promise, release };
}

test("a listing that finishes after Keep stores nothing", async () => {
  const stored = [];
  const pending = pendingListing();
  const connection = createConnection({ list: () => pending.promise, store: (config) => stored.push(config) });
  const attempt = connection.connect({ accessKeyId: "new" });
  connection.cancel();
  pending.release([{ key: "x", size: 1 }]);
  assert.equal(await attempt, null);
  assert.deepEqual(stored, []);
});

test("a listing that finishes after Forget stores nothing", async () => {
  let storage = "old";
  const pending = pendingListing();
  const connection = createConnection({ list: () => pending.promise, store: (config) => (storage = config.accessKeyId) });
  const attempt = connection.connect({ accessKeyId: "new" });
  connection.cancel();
  storage = null;
  connection.cancel();
  pending.release([]);
  assert.equal(await attempt, null);
  assert.equal(storage, null);
});

test("a newer attempt supersedes an older one whose listing arrives late", async () => {
  const stored = [];
  const first = pendingListing();
  const second = pendingListing();
  const listings = [first.promise, second.promise];
  const connection = createConnection({ list: () => listings.shift(), store: (config) => stored.push(config.accessKeyId) });
  const older = connection.connect({ accessKeyId: "older" });
  const newer = connection.connect({ accessKeyId: "newer" });
  second.release([{ key: "n", size: 1 }]);
  assert.deepEqual(await newer, [{ key: "n", size: 1 }]);
  first.release([{ key: "o", size: 1 }]);
  assert.equal(await older, null);
  assert.deepEqual(stored, ["newer"]);
});

test("a current attempt stores its key once the listing succeeds", async () => {
  const stored = [];
  const connection = createConnection({ list: async () => [], store: (config) => stored.push(config.accessKeyId) });
  assert.deepEqual(await connection.connect({ accessKeyId: "only" }), []);
  assert.deepEqual(stored, ["only"]);
  await assert.rejects(createConnection({ list: async () => { throw new Error("HTTP 403"); }, store: () => stored.push("never") }).connect({}), /403/);
  assert.deepEqual(stored, ["only"]);
});
