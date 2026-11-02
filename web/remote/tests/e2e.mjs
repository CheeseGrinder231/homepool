// Explicit end-to-end runner: real browsers at phone size against a throwaway
// Garage, the relay Worker in local workerd (serving this page) and the bridge.
// Not part of `node --test`; see docs/remote-page.md for the command.
//   HOMEPOOL_TEST_GARAGE, MINIFLARE_MODULE, CARGO_TARGET_DIR  (as for stack.mjs)
//   PLAYWRIGHT_MODULE   path to playwright's index.mjs
//   CHROMIUM_PATH       optional system Chromium
//   BROWSERS            optional, default "chromium,firefox,webkit"
//   SCREENSHOTS         optional directory for light/dark screenshots
import assert from "node:assert/strict";
import { createHash, randomBytes } from "node:crypto";
import { mkdir, readFile } from "node:fs/promises";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { crc32, deflateSync } from "node:zlib";
import { startStack } from "./stack.mjs";

const playwright = await import(pathToFileURL(process.env.PLAYWRIGHT_MODULE));
const wanted = (process.env.BROWSERS ?? "chromium,firefox,webkit").split(",");
const shots = process.env.SCREENSHOTS;
const log = (...a) => console.log(new Date().toISOString().slice(11, 19), ...a);
const sha = (b) => createHash("sha256").update(b).digest("hex");
const delay = (ms) => new Promise((r) => setTimeout(r, ms));

// ---- test files --------------------------------------------------------------

function png(width, height, pixel) {
  const raw = Buffer.alloc((width * 3 + 1) * height);
  for (let y = 0, o = 0; y < height; y++) {
    raw[o++] = 0;
    for (let x = 0; x < width; x++, o += 3) pixel(x, y, raw, o);
  }
  const chunk = (type, data) => {
    const out = Buffer.alloc(12 + data.length);
    out.writeUInt32BE(data.length, 0);
    out.write(type, 4, "ascii");
    data.copy(out, 8);
    out.writeUInt32BE(crc32(Buffer.concat([Buffer.from(type, "ascii"), data])), 8 + data.length);
    return out;
  };
  const header = Buffer.alloc(13);
  header.writeUInt32BE(width, 0);
  header.writeUInt32BE(height, 4);
  header.set([8, 2, 0, 0, 0], 8);
  return Buffer.concat([
    Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
    chunk("IHDR", header),
    chunk("IDAT", deflateSync(raw, { level: 1 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}
// A sky-and-hills picture; `grain` adds noise so the file gets large.
function picture(width, height, grain, hue = 0) {
  const noise = randomBytes(width * height);
  return png(width, height, (x, y, b, o) => {
    const hill = y > height * (0.62 + 0.08 * Math.sin((x / width) * 6 + hue));
    const sun = (x - width * 0.7) ** 2 + (y - height * 0.3) ** 2 < (height * 0.09) ** 2;
    const n = grain ? (noise[y * width + x] % grain) - grain / 2 : 0;
    const t = y / height;
    const [r, g, bl] = sun
      ? [250, 214, 120]
      : hill
        ? [40 + 30 * t + hue * 20, 110 + 40 * (1 - t), 70]
        : [90 + 80 * t, 150 + 60 * t, 220 - hue * 30];
    b[o] = Math.max(0, Math.min(255, r + n));
    b[o + 1] = Math.max(0, Math.min(255, g + n));
    b[o + 2] = Math.max(0, Math.min(255, bl + n));
  });
}
const files = {
  big: { name: "IMG_2041.png", mimeType: "image/png", buffer: picture(3000, 2000, 128) },
  small: { name: "IMG_2042.png", mimeType: "image/png", buffer: picture(1200, 900, 0, 1) },
  heic: { name: "IMG_2043.HEIC", mimeType: "image/heic", buffer: randomBytes(300 * 1024) },
  hostile: { name: '<img src=x onerror="document.title=1">.png', mimeType: "image/png", buffer: picture(640, 480, 0, 2) },
  huge: { name: "VID_long.png", mimeType: "image/png", buffer: picture(4200, 3300, 255) },
  extra: { name: "IMG_2044.png", mimeType: "image/png", buffer: picture(900, 1200, 0, 3) },
};
assert.ok(files.big.buffer.length >= 10 * 1024 * 1024, `big file ${files.big.buffer.length}`);
assert.ok(files.huge.buffer.length >= 25 * 1024 * 1024, `huge file ${files.huge.buffer.length}`);
log("files", Object.fromEntries(Object.entries(files).map(([k, f]) => [k, f.buffer.length])));

// ---- browsers ---------------------------------------------------------------------

const stack = await startStack({ log: (m) => process.env.VERBOSE && log(String(m).trim()) });
const browsers = {};
const problems = [];
async function browser(kind) {
  if (browsers[kind]) return browsers[kind];
  const type = playwright[kind];
  const options =
    kind === "chromium" && process.env.CHROMIUM_PATH
      ? { executablePath: process.env.CHROMIUM_PATH, env: { ...process.env, TMPDIR: process.env.XDG_RUNTIME_DIR } }
      : {};
  browsers[kind] = await type.launch(options);
  return browsers[kind];
}
async function phone(kind, label, colorScheme = "light") {
  const b = await browser(kind);
  const context = await b.newContext({
    viewport: { width: 390, height: 844 },
    deviceScaleFactor: 2,
    ...(kind === "chromium" ? { isMobile: true, hasTouch: true } : {}),
    colorScheme,
    acceptDownloads: true,
  });
  const page = await context.newPage();
  const sockets = { opened: 0 };
  page.on("websocket", () => sockets.opened++);
  page.on("pageerror", (e) => problems.push(`${label}: ${e.message}`));
  page.on("console", (m) => {
    // The browser logs refused sockets itself while the relay is stopped.
    if (/WebSocket connection to .* failed/.test(m.text())) return;
    if (m.type() === "error" || /Content.Security/i.test(m.text())) problems.push(`${label}: ${m.text()}`);
  });
  page.on("dialog", (d) => d.accept());
  return { kind, label, context, page, sockets };
}
const main = (p) => p.page.locator("main").innerText();
async function see(p, pattern, timeout = 30000) {
  const started = Date.now();
  for (;;) {
    const text = await main(p);
    if (pattern.test(text)) return text;
    if (Date.now() - started > timeout) throw Error(`${p.label}: never saw ${pattern}\n---\n${text}`);
    await delay(150);
  }
}
async function shot(p, name) {
  if (!shots) return;
  await mkdir(shots, { recursive: true });
  for (const scheme of ["light", "dark"]) {
    await p.page.emulateMedia({ colorScheme: scheme });
    await delay(120);
    await p.page.screenshot({ path: join(shots, `${name}-${scheme}.png`) });
  }
  await p.page.emulateMedia({ colorScheme: "light" });
}
const visible = (p, selector) => p.page.locator(selector).isVisible();
const grid = (p) => p.page.locator("#grid .tile").count();
async function upload(p, ...list) {
  await p.page.setInputFiles("#file-input", list);
}
async function rowText(p, name) {
  return p.page.locator(".upload", { hasText: name }).innerText();
}
async function waitRow(p, name, pattern, timeout = 120000) {
  const started = Date.now();
  for (;;) {
    const text = await rowText(p, name).catch(() => "");
    if (pattern.test(text)) return text;
    if (Date.now() - started > timeout) throw Error(`${p.label}: row ${name} never matched ${pattern}: ${text}`);
    await delay(100);
  }
}
async function progress(p, name, at) {
  for (;;) {
    const value = await p.page.locator(".upload", { hasText: name }).locator("progress").evaluate((e) => e.value);
    if (value >= at) return value;
    await delay(20);
  }
}
async function tile(p, name) {
  return p.page.locator("#grid .tile", { hasText: name }).first();
}
async function download(p, name) {
  await (await tile(p, name)).locator("button").click();
  await p.page.locator("#viewer").waitFor({ state: "visible" });
  const [file] = await Promise.all([p.page.waitForEvent("download", { timeout: 120000 }), p.page.click("#viewer-download")]);
  const bytes = await readFile(await file.path());
  await p.page.click("#viewer-close");
  return { sha: sha(bytes), name: file.suggestedFilename() };
}
async function stored(p) {
  return p.page.evaluate(async () => {
    const db = await new Promise((resolve, reject) => {
      const r = indexedDB.open("homepool-remote");
      r.onsuccess = () => resolve(r.result);
      r.onerror = () => reject(r.error);
    });
    const get = (k) =>
      new Promise((resolve) => {
        const r = db.transaction("records").objectStore("records").get(k);
        r.onsuccess = () => resolve(r.result);
      });
    const wrapping = await get("wrapping");
    const device = await get("device");
    let exportable = null;
    if (wrapping && device) {
      const key = await crypto.subtle.unwrapKey("pkcs8", device.wrapped, wrapping, { name: "AES-GCM", iv: device.iv }, { name: "X25519" }, false, ["deriveBits"]);
      exportable = await crypto.subtle.exportKey("pkcs8", key).then(() => true, () => false);
    }
    db.close();
    return {
      wrappingExtractable: wrapping?.extractable,
      wrappingType: wrapping?.algorithm?.name,
      device: device && { id: device.id, fields: Object.keys(device).sort() },
      exportable,
    };
  });
}
// Headless pages never become hidden on their own; switch the page's own view
// of visibility and fire the event the browser would.
async function setHidden(p, hidden) {
  await p.page.evaluate((h) => {
    Object.defineProperty(document, "hidden", { configurable: true, get: () => h });
    Object.defineProperty(document, "visibilityState", { configurable: true, get: () => (h ? "hidden" : "visible") });
    document.dispatchEvent(new Event("visibilitychange"));
  }, hidden);
}
const statusIs = (p, value) =>
  p.page
    .waitForFunction((v) => document.getElementById("status").classList.contains(v), value, { timeout: 30000 })
    .catch(async (error) => {
      const now = await p.page.evaluate(() => document.getElementById("status").className);
      throw Error(`${p.label}: status never ${value}, still "${now}"`, { cause: error });
    });
// Hidden while idle closes the session; coming back opens exactly one new one
// without a tap, and then nothing dials or pings while it sits idle.
async function hideAndReturn(p) {
  // A running transfer keeps the session while hidden, so start from idle.
  await p.page.waitForFunction(() => !document.querySelector("#grid .tile.loading"), null, { timeout: 120000 });
  await delay(300);
  await statusIs(p, "connected");
  await setHidden(p, true);
  await statusIs(p, "idle");
  const before = p.sockets.opened;
  await setHidden(p, false);
  await statusIs(p, "connected");
  assert.equal(await p.page.locator("#status").innerText(), "Connected");
  await delay(1500);
  assert.equal(p.sockets.opened, before + 1, `${p.label}: one reconnect on return`);
}
async function fingerprintOf(p) {
  await p.page.click("#paired details summary");
  return (await p.page.locator("#fingerprint").innerText()).replace(/\s+/g, "");
}

const results = {};
try {
  // ---- welcome ----------------------------------------------------------------------
  const A = await phone(wanted.includes("chromium") ? "chromium" : wanted[0], "A");
  await A.page.goto(stack.origin + "/");
  await see(A, /Pair this phone/);
  await shot(A, "01-welcome");

  // A throwaway phone whose relay replies never arrive, to photograph pairing
  // in progress; its invitation is simply left unused.
  const held = await phone(A.kind, "held");
  await held.page.routeWebSocket(/\/example$/, (ws) => {
    const server = ws.connectToServer();
    ws.onMessage((m) => server.send(m));
    server.onMessage(() => {}); // without a handler Playwright forwards
  });
  await held.page.goto(await stack.invite());
  await see(held, /Pairing this phone/, 5000);
  await shot(held, "02-pairing");
  await held.context.close();
  const inviteA = await stack.invite();
  await A.page.goto(inviteA);
  await A.page.waitForFunction(() => !location.hash, null, { timeout: 2000 });
  await see(A, /Paired/);
  await shot(A, "03-paired");
  const devices = await stack.devices();
  assert.equal(devices.length, 1);
  assert.equal(await fingerprintOf(A), devices[0].fingerprint, "page fingerprint matches home");
  await A.page.click("#paired-continue");
  await see(A, /No photos yet/);
  await shot(A, "04-empty");
  // The link's entry in history no longer carries the fragment either.
  await A.page.goBack();
  await A.page.goForward();
  assert.equal(new URL(A.page.url()).hash, "");
  await see(A, /No photos yet/);
  log("pairing from the link, fragment wipe and fingerprint passed");

  // ---- upload, list, preview, download -------------------------------------------------
  await upload(A, files.big, files.small, files.heic, files.hostile);
  await progress(A, "IMG_2041", 0.3);
  await shot(A, "05-uploading");
  for (const f of [files.big, files.small, files.heic, files.hostile]) await waitRow(A, f.name.slice(0, 8), /Added/);
  assert.equal(await A.page.evaluate(() => document.querySelectorAll('img[src="x"]').length), 0, "hostile name stays text");
  assert.equal(await A.page.title(), "Homepool photos");
  await A.page.click("#refresh");
  await see(A, /4 photos/);
  await delay(1500);
  await shot(A, "06-grid");
  // HEIC is not renderable in Chromium/Firefox: a file tile that still downloads.
  assert.ok(await (await tile(A, "IMG_2043")).evaluate((e) => e.classList.contains("file")));
  results.heic = await download(A, "IMG_2043");
  assert.equal(results.heic.sha, sha(files.heic.buffer));
  await (await tile(A, "IMG_2041")).locator("button").click();
  await A.page.locator("#viewer-image").waitFor({ state: "visible", timeout: 60000 });
  await shot(A, "07-preview");
  await A.page.click("#viewer-close");
  results.bigA = await download(A, "IMG_2041");
  assert.equal(results.bigA.sha, sha(files.big.buffer), "10 MiB download checksum");
  results.smallA = await download(A, "IMG_2042");
  assert.equal(results.smallA.sha, sha(files.small.buffer));
  log("upload, list, preview and checksum download passed", results.bigA);

  // ---- reload: still paired through the wrapped key ---------------------------------------
  await A.page.reload();
  await see(A, /4 photos/);
  const storedA = await stored(A);
  assert.equal(storedA.wrappingExtractable, false);
  assert.equal(storedA.wrappingType, "AES-GCM");
  assert.equal(storedA.exportable, false, "unwrapped device key is not extractable");
  assert.deepEqual(storedA.device.fields, ["bridge", "id", "iv", "paired", "publicKey", "route", "token", "wrapped"]);
  log("reload stays paired via unwrapKey", storedA);

  // ---- second phone at the same time --------------------------------------------------------
  const B = await phone(wanted.includes("firefox") ? "firefox" : wanted[0], "B");
  // B pairs from a pasted link; once paired, the page keeps no copy of it.
  await B.page.goto(stack.origin + "/");
  await see(B, /Pair this phone/);
  await B.page.click("details.paste summary");
  await B.page.fill("#paste-text", await stack.invite());
  await B.page.click("#paste-form button[type=submit]");
  await see(B, /Paired/);
  assert.equal(await B.page.inputValue("#paste-text"), "", "pasted link cleared after pairing");
  await B.page.click("#paired-continue");
  await see(B, /4 photos/);
  // Both sessions live: B uploads while A downloads the large photo.
  const [bigB] = await Promise.all([
    download(A, "IMG_2041"),
    (async () => {
      await upload(B, files.extra);
      await waitRow(B, "IMG_2044", /Added/);
    })(),
  ]);
  assert.equal(bigB.sha, sha(files.big.buffer));
  results.bigB = await download(B, "IMG_2041");
  assert.equal(results.bigB.sha, sha(files.big.buffer));
  await A.page.click("#refresh");
  await see(A, /5 photos/);
  assert.equal((await stack.devices()).length, 2);
  log(`two phones (${A.kind}, ${B.kind}) paired and working at once passed`);

  // ---- visibility: hidden closes the session, returning reconnects once ----------------------
  await hideAndReturn(A);
  await hideAndReturn(B);
  await A.page.click("#refresh");
  await see(A, /5 photos/);
  log("hidden page closes its session; return reconnects once without a tap");

  // ---- interrupted uploads ------------------------------------------------------------------
  // Relay dies mid-upload: the page says it is not confirmed, then asks home.
  await upload(A, files.huge);
  await progress(A, "VID_long", 0.2);
  await stack.stopRelay();
  await waitRow(A, "VID_long", /Not confirmed\. Can't reach the relay/);
  await stack.startRelay();
  let status;
  // Each try is a relay connection; keep well inside the daily upgrade budget.
  for (let i = 0; i < 15; i++) {
    await delay(3000);
    await A.page.locator(".upload", { hasText: "VID_long" }).locator(".upload-action").click();
    status = await waitRow(A, "VID_long", /Not confirmed|failed|Added/i);
    if (!/relay|isn't connected|connect/.test(status)) break;
  }
  const journal1 = Object.values(await stack.journal()).filter((e) => e.name === "VID_long.png");
  assert.equal(journal1.length, 1);
  const expected1 = { pending: /Not confirmed yet/, failed: /Upload failed/, committed: /Added/ }[journal1[0].status];
  assert.match(status, expected1, `page shows the journal outcome ${journal1[0].status}`);
  await shot(A, journal1[0].status === "pending" ? "08-not-confirmed" : "08-relay-outcome");
  log("relay killed mid-upload: page outcome matches journal", journal1[0].status);
  // A pending upload is settled as failed by home (at once on a current
  // bridge, or on restart); then it can be sent again under a fresh ID.
  if (journal1[0].status === "pending") {
    await stack.killBridge();
    await stack.startBridge();
    await A.page.locator(".upload", { hasText: "VID_long" }).locator(".upload-action").click();
  }
  await waitRow(A, "VID_long", /Upload failed/);
  await shot(A, "09-upload-failed");
  await A.page.locator(".upload", { hasText: "VID_long" }).locator(".upload-action").click();
  await waitRow(A, "VID_long", /Added/, 180000);
  const journal2 = Object.values(await stack.journal()).filter((e) => e.name === "VID_long.png");
  assert.deepEqual(journal2.map((e) => e.status).sort(), ["committed", "failed"], "fresh ID for the retry");
  log("upload failed after restart, fresh upload committed");

  // Bridge dies mid-upload.
  await upload(A, { ...files.huge, name: "VID_second.png" });
  await progress(A, "VID_second", 0.2);
  await stack.killBridge();
  await waitRow(A, "VID_second", /Not confirmed\. Your pool at home isn't connected/);
  await shot(A, "10-not-confirmed-home-offline");
  await stack.startBridge();
  await A.page.locator(".upload", { hasText: "VID_second" }).locator(".upload-action").click();
  await waitRow(A, "VID_second", /Upload failed/);
  const journal3 = Object.values(await stack.journal()).filter((e) => e.name === "VID_second.png");
  assert.deepEqual(journal3.map((e) => e.status), ["failed"]);
  log("bridge killed mid-upload: put-status failed after reconciliation");

  // Store confirmed the write but home died before saying so: committed.
  stack.hold.put = true;
  stack.hold.confirmed = false;
  await upload(A, { ...files.small, name: "IMG_held.png" });
  while (!stack.hold.confirmed) await delay(50);
  await stack.killBridge();
  stack.hold.put = false;
  await waitRow(A, "IMG_held", /Not confirmed/);
  await stack.startBridge();
  await A.page.locator(".upload", { hasText: "IMG_held" }).locator(".upload-action").click();
  await waitRow(A, "IMG_held", /Added/);
  log("store-confirmed upload before journal commit: committed after restart");

  // ---- refusals keep the existing pairing ---------------------------------------------------------
  const before = (await stored(A)).device.id;
  const refusals = {};
  const tryLink = async (p, link, expect) => {
    await p.page.goto(link);
    await p.page.waitForFunction(() => !location.hash, null, { timeout: 2000 });
    if (await visible(p, "#replace")) await p.page.click("#replace-accept");
    const text = await see(p, expect, 40000);
    return text.match(expect)[0];
  };
  refusals.malformed = await tryLink(A, `${stack.origin}/#route=example&token=zz`, /isn't a complete pairing code/);
  refusals.hostile = await tryLink(
    A,
    `${inviteA}&x=${encodeURIComponent("<script>alert(1)</script>")}`,
    /isn't a complete pairing code/,
  );
  const fresh = await stack.invite();
  const wrongKey = fresh.replace(/bridge=([0-9a-f])/, (_, c) => `bridge=${c === "0" ? "1" : "0"}`);
  refusals.wrongKey = await tryLink(A, wrongKey, /pairing code didn't work/);
  await shot(A, "11-code-refused");
  refusals.used = await tryLink(A, inviteA, /pairing code didn't work/);
  const expiring = await stack.invite();
  await stack.killBridge();
  const invitationId = new URLSearchParams(expiring.split("#")[1]).get("invitation");
  await stack.editState((s) => {
    s.invitations[invitationId].expiry = Math.floor(Date.now() / 1000) - 1;
  });
  await stack.startBridge();
  refusals.expired = await tryLink(A, expiring, /pairing code didn't work/);
  await A.page.goto(stack.origin + "/");
  await see(A, /photos ·/);
  assert.equal((await stored(A)).device.id, before, "existing pairing kept after every refusal");
  log("refusals keep the pairing", refusals);

  // ---- WebKit: the Safari-relevant wrapped-key path ------------------------------------------------
  if (wanted.includes("webkit")) {
    const W = await phone("webkit", "W");
    // The wrong-key attempt above did not use up this invitation.
    await W.page.goto(fresh);
    await see(W, /Paired/);
    await W.page.click("#paired-continue");
    await see(W, /photos ·/);
    await W.page.reload();
    await see(W, /photos ·/);
    await hideAndReturn(W);
    const storedW = await stored(W);
    assert.equal(storedW.exportable, false);
    await upload(W, { ...files.small, name: "IMG_webkit.png" });
    await waitRow(W, "IMG_webkit", /Added/);
    results.webkit = await download(W, "IMG_2041");
    assert.equal(results.webkit.sha, sha(files.big.buffer));
    log("webkit: paired, reload via unwrapKey, upload and checksum download passed");
    await W.context.close();
  } else {
    await B.page.goto(fresh);
    await see(B, /already paired/);
    await B.page.click("#replace-keep");
  }

  // ---- forget -------------------------------------------------------------------------------
  await B.page.click("#forget");
  await see(B, /no longer paired/);
  await shot(B, "12-forgotten");
  const left = await B.page.evaluate(async () =>
    indexedDB.databases ? (await indexedDB.databases()).map((d) => d.name) : null,
  );
  assert.ok(!left || !left.includes("homepool-remote"), `database removed: ${left}`);
  await B.page.reload();
  await see(B, /Pair this phone/);
  log("forget deletes local pairing", left);

  // ---- revoke at home -------------------------------------------------------------------------
  const deviceA = (await stack.devices()).find((d) => d.id === before);
  assert.deepEqual(await stack.revoke(deviceA.id), { revoked: true });
  await delay(500);
  await A.page.click("#refresh");
  await see(A, /removed at home/, 30000);
  await shot(A, "13-removed");
  log("revoke at home shows unpaired");

  assert.deepEqual(problems, [], "no page errors or CSP violations");
  console.log(JSON.stringify({ ok: true, browsers: Object.keys(browsers), results, refusals }, null, 1));
} finally {
  for (const b of Object.values(browsers)) await b.close().catch(() => {});
  await stack.stop();
}
