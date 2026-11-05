// Explicit end-to-end runner for the read-only service tabs (#59): real
// browsers at phone size against a throwaway Garage, the relay Worker in
// local workerd (serving this page), the bridge, and a small fixture service
// on loopback that serves view documents. Not part of `node --test`; see
// docs/remote-page.md for the command. Same environment as e2e.mjs:
//   HOMEPOOL_TEST_GARAGE, MINIFLARE_MODULE, CARGO_TARGET_DIR, PLAYWRIGHT_MODULE
//   CHROMIUM_PATH, BROWSERS (default "chromium,firefox,webkit"), SCREENSHOTS
import assert from "node:assert/strict";
import { mkdir } from "node:fs/promises";
import { createServer } from "node:http";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { startStack } from "./stack.mjs";

const playwright = await import(pathToFileURL(process.env.PLAYWRIGHT_MODULE));
const wanted = (process.env.BROWSERS ?? "chromium,firefox,webkit").split(",");
const shots = process.env.SCREENSHOTS;
const log = (...a) => console.log(new Date().toISOString().slice(11, 19), ...a);
const delay = (ms) => new Promise((r) => setTimeout(r, ms));
const https = (rest) => ["https", `//${rest}`].join(":");
const iso = (ms) => new Date(ms).toISOString().replace(/\.\d+Z$/, "Z");

// ---- the fixture service ------------------------------------------------------------

const PAGE_REFRESH = 30; // the shortest refresh_s a document may ask for
function healthy(now = Date.now()) {
  return {
    version: 1,
    title: "Work status",
    as_of: iso(now - 20000),
    valid_until: iso(now + 10 * 60000),
    refresh_s: PAGE_REFRESH,
    state: "ok",
    notes: ["Nightly jobs finished at 02:10."],
    sections: [
      {
        title: "Builds",
        state: "ok",
        summary: "Every pipeline passed.",
        items: [
          {
            title: "Main branch",
            state: "ok",
            detail: "Build 412 passed in 6 minutes.",
            meta: ["build 412", "6 min"],
            link: https("example.org/builds/412"),
            updated: iso(now - 4 * 60000),
          },
          { title: "Release checklist", state: "ok", progress: { value: 5, target: 5 } },
        ],
      },
      {
        title: "Reviews",
        state: "ok",
        items: [{ title: "Waiting for review", state: "ok", detail: "2 changes, both under a day old.", progress: { value: 4, target: 6 } }],
      },
      { title: "Backups", state: "ok", items: [{ title: "Photos", state: "ok", meta: ["last night"] }] },
    ],
  };
}
function attention(now = Date.now()) {
  const doc = healthy(now);
  doc.state = "warn";
  doc.sections = [
    doc.sections[2],
    {
      title: "Builds",
      state: "warn",
      summary: "One pipeline is slow.",
      items: [
        doc.sections[0].items[0],
        { title: "Nightly", state: "warn", detail: "Took 48 minutes; it usually takes 20.", meta: ["build 411"], updated: iso(now - 7 * 3600000) },
      ],
    },
    {
      title: "Deploys",
      state: "stuck",
      items: [
        {
          title: "Staging",
          state: "stuck",
          detail: "Waiting for approval since 09:12.",
          link: https("example.org/deploys/88"),
          progress: { value: 3, target: 5 },
          updated: iso(now - 95 * 60000),
        },
      ],
    },
  ];
  return doc;
}
const HOSTILE = '<img src=x onerror="document.title=\'pwned\'"><script>document.title="pwned"</script>';
function hostile(now = Date.now()) {
  const doc = healthy(now);
  doc.title = HOSTILE;
  doc.sections[0].items[0] = {
    title: HOSTILE,
    state: "error",
    detail: HOSTILE,
    meta: [HOSTILE],
    link: ["javascript", "document.title='pwned'"].join(":"),
  };
  doc.sections[0].items[1].link = ["data", "text/html,<script>alert(1)</script>"].join(":");
  return doc;
}

const fixture = {
  mode: "healthy",
  hits: new Map(), // path -> count
  server: null,
  port: 0,
  respond(req, res) {
    this.hits.set(req.url, (this.hits.get(req.url) ?? 0) + 1);
    const send = (status, type, body) => {
      res.writeHead(status, { "Content-Type": type, "Content-Length": Buffer.byteLength(body) });
      res.end(body);
    };
    const doc = (d) => send(200, "application/json", JSON.stringify(d));
    if (this.mode === "healthy") return doc(healthy());
    if (this.mode === "attention") return doc(attention());
    if (this.mode === "hostile") return doc(hostile());
    if (this.mode === "already-expired") {
      const d = healthy();
      d.valid_until = iso(Date.now() - 3 * 60000);
      d.notes = ["One source expired before publication."];
      return doc(d);
    }
    if (this.mode === "expiring") {
      const d = healthy();
      d.valid_until = iso(Date.now() + 4000);
      return doc(d);
    }
    if (this.mode === "big") return send(200, "application/json", JSON.stringify(healthy()) + " ".repeat(70000));
    if (this.mode === "invalid") return send(200, "text/html", "<html><body>Status</body></html>");
    if (this.mode === "redirect") {
      res.writeHead(302, { Location: https("example.org/"), "Content-Length": 0 });
      return res.end();
    }
  },
  async start(host) {
    this.server = createServer((req, res) => this.respond(req, res));
    await new Promise((r) => this.server.listen(this.port, host, r));
    this.port = this.server.address().port;
  },
  async stop() {
    this.server.closeAllConnections();
    await new Promise((r) => this.server.close(r));
  },
  count(path) {
    return this.hits.get(path) ?? 0;
  },
};

// ---- browsers -----------------------------------------------------------------------------

const stack = await startStack({ log: (m) => process.env.VERBOSE && log(String(m).trim()) });
await fixture.start(stack.loopback);
const serviceUrl = (path) => ["http", `//${stack.loopback}:${fixture.port}${path}`].join(":");
const browsers = {};
const problems = [];
async function browser(kind) {
  if (browsers[kind]) return browsers[kind];
  const options =
    kind === "chromium" && process.env.CHROMIUM_PATH
      ? { executablePath: process.env.CHROMIUM_PATH, env: { ...process.env, TMPDIR: process.env.XDG_RUNTIME_DIR } }
      : {};
  browsers[kind] = await playwright[kind].launch(options);
  return browsers[kind];
}
async function phone(kind, label) {
  const context = await (await browser(kind)).newContext({
    viewport: { width: 390, height: 844 },
    deviceScaleFactor: 2,
    ...(kind === "chromium" ? { isMobile: true, hasTouch: true } : {}),
    colorScheme: "light",
  });
  const page = await context.newPage();
  const sockets = { opened: 0 };
  page.on("websocket", () => sockets.opened++);
  page.on("pageerror", (e) => problems.push(`${label}: ${e.message}`));
  page.on("console", (m) => {
    if (/WebSocket connection to .* failed/.test(m.text())) return;
    // WebKit screenshots inject a style of Playwright's own, which the page's
    // CSP refuses; only that message, and only while a screenshot is taken.
    if (shooting && /^Refused to apply a stylesheet/.test(m.text())) return;
    if (m.type() === "error" || /Content.Security/i.test(m.text())) problems.push(`${label}: ${m.text()}`);
  });
  page.on("dialog", (d) => d.accept());
  return { kind, label, context, page, sockets };
}
const text = (p) => p.page.locator("main").innerText();
async function see(p, pattern, timeout = 30000) {
  const started = Date.now();
  for (;;) {
    const t = await text(p);
    if (pattern.test(t)) return t;
    if (Date.now() - started > timeout) throw Error(`${p.label}: never saw ${pattern}\n---\n${t}`);
    await delay(150);
  }
}
let shooting = false;
async function shot(p, name) {
  if (!shots) return;
  shooting = true;
  await mkdir(shots, { recursive: true });
  for (const scheme of ["light", "dark"]) {
    await p.page.emulateMedia({ colorScheme: scheme });
    await delay(150);
    await p.page.screenshot({ path: join(shots, `${p.kind}-${name}-${scheme}.png`), fullPage: true });
  }
  await p.page.emulateMedia({ colorScheme: "light" });
  await delay(200); // console messages arrive after the screenshot resolves
  shooting = false;
}
async function setHidden(p, hidden) {
  await p.page.evaluate((h) => {
    Object.defineProperty(document, "hidden", { configurable: true, get: () => h });
    Object.defineProperty(document, "visibilityState", { configurable: true, get: () => (h ? "hidden" : "visible") });
    document.dispatchEvent(new Event("visibilitychange"));
  }, hidden);
}
const tabNames = (p) => p.page.locator("#tabs [role=tab]").allInnerTexts();
async function pair(p) {
  await p.page.goto(await stack.invite());
  await see(p, /Paired/);
  await p.page.click("#paired-continue");
  await see(p, /No photos yet|photos ·/);
  return (await stack.devices()).at(-1).id;
}
async function openTab(p, name) {
  await p.page.locator("#tabs [role=tab]", { hasText: name }).click();
}
// Refresh, then wait until that fetch has finished one way or the other.
async function refresh(p) {
  await p.page.waitForFunction(() => document.getElementById("service").getAttribute("aria-busy") !== "true");
  await p.page.click("#refresh");
  await settled(p);
}
const settled = (p) =>
  p.page.waitForFunction(() => document.getElementById("service").getAttribute("aria-busy") === "false", null, {
    timeout: 60000,
  });
async function waitHits(path, n, timeout = 30000) {
  const started = Date.now();
  while (fixture.count(path) < n) {
    if (Date.now() - started > timeout) throw Error(`${path}: ${fixture.count(path)} fetches, wanted ${n}`);
    await delay(100);
  }
}

const results = {};
try {
  // ---- empty catalog: no tabs at all -----------------------------------------------
  const A = await phone(wanted.includes("chromium") ? "chromium" : wanted[0], "A");
  const deviceA = await pair(A);
  assert.equal(await A.page.locator("#tabs").isVisible(), false, "no tabs without services");
  assert.equal(await A.page.locator("#heading").innerText(), "Homepool photos");
  await shot(A, "01-empty-catalog");
  log("empty catalog: no tabs");

  // ---- the catalog: Status for every phone, Work for phone A only ----------------------
  await stack.configure([
    { id: "status", title: "Status", url: serviceUrl("/status.json"), max_bytes: 65536, devices: "all" },
    { id: "work", title: "Work", url: serviceUrl("/work.json"), devices: [deviceA] },
  ]);
  await stack.killBridge();
  await stack.startBridge();
  await A.page.reload();
  await A.page.locator("#tabs").waitFor({ state: "visible", timeout: 30000 });
  assert.deepEqual(await tabNames(A), ["Photos", "Status", "Work"]);
  assert.equal(await A.page.locator("#heading").innerText(), "Homepool");
  assert.equal(fixture.count("/status.json"), 0, "listing tabs fetches no document");
  await shot(A, "02-tabs");

  // ---- a healthy document -------------------------------------------------------------
  await openTab(A, "Status");
  await see(A, /Work status/);
  await see(A, /Updated just now/);
  const body = await text(A);
  for (const s of ["Every pipeline passed.", "Build 412 passed", "5 of 5", "4 of 6", "build 412", "Nightly jobs finished", "4 OK"])
    assert.ok(body.includes(s), `healthy shows ${s}`);
  const link = A.page.locator("a.svc-link").first();
  assert.equal(await link.getAttribute("href"), https("example.org/builds/412"));
  assert.equal(await link.getAttribute("rel"), "noopener noreferrer");
  assert.equal(await link.getAttribute("target"), "_blank");
  assert.equal(await A.page.locator("#service-state").innerText(), "OK");
  assert.equal(fixture.count("/status.json"), 1, "one fetch when the tab opens");
  await shot(A, "03-healthy");
  log("healthy document with sections, items, links and progress");

  // ---- stuck and warn come first ------------------------------------------------------
  fixture.mode = "attention";
  await refresh(A);
  await see(A, /Waiting for approval/);
  const sections = await A.page.locator(".svc-section-title").allInnerTexts();
  assert.deepEqual(sections, ["Deploys", "Builds", "Backups"], "worst section first");
  const builds = await A.page.locator(".svc-section", { hasText: "Builds" }).locator(".svc-item-title").allInnerTexts();
  assert.deepEqual(builds, ["Nightly", "Main branch"], "worst item first");
  assert.match(await A.page.locator("#service-counts").innerText(), /1 stuck · 1 warning · 2 OK/);
  assert.equal(await A.page.locator(".svc-section", { hasText: "Backups" }).evaluate((d) => d.open), false, "all-OK section folded");
  assert.equal(await A.page.locator("#tabs [role=tab]", { hasText: "Status" }).getAttribute("data-state"), "stuck");
  await shot(A, "04-attention");
  log("stuck and warn items first, OK folded");

  // ---- stale past valid_until, without a fetch ---------------------------------------------
  fixture.mode = "expiring";
  await refresh(A);
  const fetched = await waitHits("/status.json", 3).then(() => fixture.count("/status.json"));
  await see(A, /Updated just now/);
  assert.equal(await A.page.locator("#service-stale").isVisible(), false);
  await see(A, /Out of date/, 20000);
  assert.equal(fixture.count("/status.json"), fetched, "turned stale by the clock, not a fetch");
  assert.equal(await A.page.locator("#service-state").innerText(), "Out of date");
  // The age names an old snapshot, never an update.
  assert.match(await A.page.locator("#service-age").innerText(), /^Snapshot from \d.* · Expired (just now|\d+ min ago)/);
  assert.match(await A.page.locator("#service-age").innerText(), /^(?!.*Updated)/);
  assert.match(await A.page.locator("#service-stale").innerText(), /said this was good until \d/);
  assert.match(await A.page.locator("#service-counts").innerText(), /^When last sent: 4 OK$/);
  assert.equal(await A.page.locator(".badge.s-ok", { hasText: /^OK$/ }).count(), 0, "no OK badge on stale data");
  assert.ok((await A.page.locator(".badge.s-ok").allInnerTexts()).every((t) => t === "Was OK"));
  const green = await A.page.locator(".badge.s-ok").first().evaluate((e) => getComputedStyle(e).color);
  const muted = await A.page.locator("#service-age").evaluate((e) => getComputedStyle(e).color);
  assert.equal(green, muted, "stale OK is grey, not green");
  await shot(A, "05-stale");
  log("stale past valid_until: labelled out of date, never green");

  // ---- published with a source deadline that already passed -------------------------------
  fixture.mode = "already-expired";
  await refresh(A);
  await settled(A);
  assert.equal(await A.page.locator("#service-state").innerText(), "Out of date");
  assert.equal(await A.page.locator("#service").evaluate((e) => e.classList.contains("is-stale")), true);
  assert.match(await A.page.locator("#service-age").innerText(), /^Snapshot from .* · Expired 3 min ago$/);
  assert.ok((await text(A)).includes("Build 412 passed"), "expired document content is retained");
  assert.ok((await text(A)).includes("One source expired before publication."));
  assert.equal(await A.page.locator("#service-problem").isVisible(), false);
  assert.equal(await A.page.locator(".badge.s-ok", { hasText: /^OK$/ }).count(), 0);
  assert.equal(await A.page.locator(".badge.s-ok").first().evaluate((e) => getComputedStyle(e).color), muted);
  assert.equal(await A.page.locator("#tabs [role=tab]", { hasText: "Status" }).getAttribute("data-state"), "stale");
  await shot(A, "05-already-expired");
  log("already expired at publication: content shown grey and out of date");

  // ---- the service stops, then comes back ----------------------------------------------------
  fixture.mode = "healthy";
  await refresh(A);
  await see(A, /Updated just now/);
  await fixture.stop();
  await refresh(A);
  await see(A, /Status isn't answering at home right now\. Check that it's running on the home computer, then try again\. Below is what it sent/);
  await shot(A, "06-down-last-known");
  await openTab(A, "Work");
  await see(A, /Work isn't answering at home right now/);
  assert.equal(await A.page.locator("#service-view").isVisible(), false);
  await shot(A, "07-down");
  await fixture.start(stack.loopback);
  await A.page.click("#service-retry");
  await settled(A);
  await see(A, /Work status/);
  assert.equal(await A.page.locator("#service-problem").isVisible(), false);
  log("service down, then back after Try again");

  // ---- refused documents: oversize, invalid, redirect; hostile shown as text -----------------
  await openTab(A, "Status");
  results.refused = {};
  for (const [mode, pattern] of [
    ["big", /Status sent more than its size limit/],
    ["invalid", /Status sent something this page can't read/],
    ["redirect", /Status isn't answering at home/],
  ]) {
    fixture.mode = mode;
    await refresh(A);
    results.refused[mode] = (await see(A, pattern)).match(pattern)[0];
  }
  await shot(A, "08-invalid");
  fixture.mode = "hostile";
  await refresh(A);
  await A.page.waitForFunction(() => document.getElementById("service-title").textContent.includes("<img"));
  assert.ok((await A.page.locator("#service-title").innerText()).includes("<script>"), "title is text");
  assert.ok((await text(A)).includes('<img src=x onerror="document.title'), "item is text");
  assert.equal(await A.page.title(), "Homepool photos", "nothing ran");
  assert.equal(await A.page.locator("#service img, #service script").count(), 0);
  assert.equal(await A.page.locator("#service a").count(), 0, "javascript: and data: links dropped");
  await shot(A, "09-hostile");
  log("oversize, invalid and redirect refused; hostile strings shown as text", results.refused);

  // ---- hidden: no fetch; return: exactly one; then every refresh_s -----------------------------
  fixture.mode = "healthy";
  await refresh(A);
  await see(A, /Updated just now/);
  await A.page.waitForFunction(() => document.getElementById("status").classList.contains("connected"));
  await setHidden(A, true);
  const hiddenAt = fixture.count("/status.json");
  const socketsBefore = A.sockets.opened;
  await delay((PAGE_REFRESH + 5) * 1000);
  assert.equal(fixture.count("/status.json"), hiddenAt, "nothing fetched while hidden");
  await setHidden(A, false);
  await waitHits("/status.json", hiddenAt + 1);
  await delay(3000);
  assert.equal(fixture.count("/status.json"), hiddenAt + 1, "one fetch on return");
  assert.equal(A.sockets.opened, socketsBefore + 1, "one session on return");
  await waitHits("/status.json", hiddenAt + 2, (PAGE_REFRESH + 8) * 1000);
  assert.equal(A.sockets.opened, socketsBefore + 1, "refresh reuses the session");
  log("hidden: no fetch; return: one fetch and one session; then one per refresh_s");

  // ---- after hiding, kept data is never current until home answers -----------------------------
  // The phone may have slept while hidden; if the refresh on return fails,
  // the kept document stays "Last known" with no age and no green.
  await settled(A);
  await see(A, /Updated just now/);
  await setHidden(A, true);
  fixture.mode = "redirect";
  await setHidden(A, false);
  await see(A, /Last known/);
  await settled(A);
  await see(A, /Status isn't answering at home right now\. .* Below is the last thing it sent; this phone can't tell how long ago\./);
  assert.equal(await A.page.locator("#service-state").innerText(), "Last known");
  assert.match(await A.page.locator("#service-age").innerText(), /^Snapshot from \d.* · Age not known$/);
  assert.match(await A.page.locator("#service-stale").innerText(), /can't tell how old this is/);
  assert.equal(await A.page.locator(".badge.s-ok", { hasText: /^OK$/ }).count(), 0, "no OK badge on doubted data");
  const doubted = await A.page.locator(".badge.s-ok").first().evaluate((e) => getComputedStyle(e).color);
  assert.equal(doubted, await A.page.locator("#service-age").evaluate((e) => getComputedStyle(e).color), "grey, not green");
  assert.equal(await A.page.locator("#tabs [role=tab]", { hasText: "Status" }).getAttribute("data-state"), "stale");
  await shot(A, "11-last-known");
  fixture.mode = "healthy";
  await A.page.click("#service-retry");
  await settled(A);
  await see(A, /Updated just now/);
  assert.equal(await A.page.locator("#service-state").innerText(), "OK");
  log("after hiding with a failed refresh: last known, no age, never green; current again once home answers");

  // ---- a phone without the Work grant ---------------------------------------------------------
  const B = await phone(wanted.includes("firefox") ? "firefox" : wanted[0], "B");
  await pair(B);
  await B.page.locator("#tabs").waitFor({ state: "visible", timeout: 30000 });
  assert.deepEqual(await tabNames(B), ["Photos", "Status"], "no tab without a grant");
  await openTab(B, "Status");
  await see(B, /Work status/);
  await shot(B, "10-second-phone");
  log(`second phone (${B.kind}) without the Work grant sees only Status`);

  // ---- WebKit renders the same document --------------------------------------------------------
  if (wanted.includes("webkit")) {
    const W = await phone("webkit", "W");
    await pair(W);
    await W.page.locator("#tabs").waitFor({ state: "visible", timeout: 30000 });
    await openTab(W, "Status");
    fixture.mode = "attention";
    await refresh(W);
    await see(W, /Waiting for approval/);
    await shot(W, "11-attention");
    await W.context.close();
    log("webkit: tabs and document");
  }

  // ---- the last tab is remembered across a reload -----------------------------------------------
  await A.page.reload();
  await see(A, /Work status/);
  assert.equal(await A.page.locator("#tabs [aria-selected=true]").innerText(), "Status");

  // ---- revoke at home ends access ---------------------------------------------------------------
  assert.deepEqual(await stack.revoke(deviceA), { revoked: true });
  await delay(500);
  const before = fixture.count("/status.json");
  await A.page.click("#refresh");
  await see(A, /removed at home/, 30000);
  assert.equal(await A.page.locator("#tabs").isVisible(), false);
  assert.equal(fixture.count("/status.json"), before, "a revoked phone reaches no service");
  await shot(A, "12-revoked");
  log("revoke: removed screen, tabs gone, no fetch");

  assert.deepEqual(problems, [], "no page errors or CSP violations");
  console.log(JSON.stringify({ ok: true, browsers: Object.keys(browsers), hits: Object.fromEntries(fixture.hits), results }, null, 1));
} finally {
  for (const b of Object.values(browsers)) await b.close().catch(() => {});
  await fixture.stop().catch(() => {});
  await stack.stop();
}
