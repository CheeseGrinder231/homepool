// The remote page served from static assets, and the phone admission close
// codes the page turns into plain messages.
import { test } from "node:test";
import assert from "node:assert/strict";
import { harness } from "./harness.mjs";
import { admission, tokenSet } from "../../web/protocol/wire.mjs";
import { hash } from "../limits.mjs";
import { PHONE_CLOSE } from "../route.mjs";
import worker, { PAGE, PAGE_HEADERS } from "../worker.mjs";
import { CLOSE } from "../../web/remote/client.mjs";
import { readFile } from "node:fs/promises";

const unhex = (s) => new Uint8Array(Buffer.from(s, "hex"));
const web = new URL("../../web/", import.meta.url);
const origin = ["https", "//192.0.2.1"].join(":");

function env() {
  const requests = [];
  return {
    requests,
    KILL_SWITCH: "false",
    ROUTES: '["example"]',
    // Static assets: the file under web/, or 404; ETag "x" answers 304.
    ASSETS: {
      async fetch(request) {
        requests.push(request);
        const path = new URL(request.url).pathname;
        if (request.headers.get("If-None-Match") === '"x"')
          return new Response(null, { status: 304, headers: { ETag: '"x"' } });
        try {
          const body = await readFile(new URL("." + path, web));
          return new Response(request.method === "HEAD" ? null : body, {
            headers: { ETag: '"x"', "Content-Type": "application/octet-stream" },
          });
        } catch {
          return new Response("missing", { status: 404 });
        }
      },
    },
    UPGRADES: { idFromName: () => assert.fail("no gate"), get: () => assert.fail("no gate") },
    ROUTES_DO: { idFromName: () => assert.fail("no route"), get: () => assert.fail("no route") },
  };
}
const fetchPage = (e, path, init) => worker.fetch(new Request(origin + path, init), e);

test("the close codes the page reads match the relay's", () => {
  assert.deepEqual({ ...CLOSE }, { ...PHONE_CLOSE });
});

test("the page is served only from its fixed list, with its headers", async () => {
  const e = env();
  const index = await fetchPage(e, "/");
  assert.equal(index.status, 200);
  assert.match(await index.text(), /<script type="module" src="\/remote\/app.mjs">/);
  assert.equal(e.requests.at(-1).url, origin + "/remote/index.html");
  assert.equal(index.headers.get("Content-Type"), "text/html; charset=utf-8");
  for (const [name, value] of Object.entries(PAGE_HEADERS)) assert.equal(index.headers.get(name), value);
  const csp = index.headers.get("Content-Security-Policy");
  assert.match(csp, /script-src 'self'/);
  assert.match(csp, /connect-src 'self'/);
  assert.match(csp, /img-src 'self' blob:/);
  assert.doesNotMatch(csp, /unsafe|\*|data:/);
  for (const path of PAGE.keys()) {
    const r = await fetchPage(env(), path);
    assert.equal(r.status, 200, path);
    const type = r.headers.get("Content-Type");
    if (path.endsWith(".mjs")) assert.equal(type, "text/javascript; charset=utf-8");
    assert.equal(r.headers.get("X-Content-Type-Options"), "nosniff");
  }
  // Every module the page imports is on the list.
  for (const path of PAGE.keys()) {
    if (!path.endsWith(".mjs")) continue;
    const source = await readFile(new URL("." + PAGE.get(path), web), "utf8");
    for (const [, spec] of source.matchAll(/^import[^"]*"([^"]+)"/gm)) {
      const target = new URL(spec, origin + path).pathname;
      assert.ok(PAGE.has(target), `${path} imports ${target}`);
    }
  }
  const head = await fetchPage(e, "/", { method: "HEAD" });
  assert.equal(head.status, 200);
  const cached = await fetchPage(e, "/remote/app.mjs", { headers: { "If-None-Match": '"x"' } });
  assert.equal(cached.status, 304);
  assert.equal(cached.headers.get("ETag"), '"x"');
  assert.equal(cached.headers.get("Content-Security-Policy"), PAGE_HEADERS["Content-Security-Policy"]);
});

test("nothing else is served: tests, the LAN page, other paths and methods", async () => {
  for (const path of [
    "/index.html",
    "/remote/",
    "/remote/index.html",
    "/remote/tests/e2e.mjs",
    "/images/app.mjs",
    "/images/sigv4.mjs",
    "/images/index.html",
    "/protocol/tests/noise.test.mjs",
    "/remote/../images/sigv4.mjs",
    "/REMOTE/app.mjs",
    "/favicon.ico",
  ]) {
    const e = env();
    const r = await fetchPage(e, path);
    assert.equal(r.status, 404, path);
    assert.equal(e.requests.length, 0, path);
  }
  const e = env();
  assert.equal((await fetchPage(e, "/?x=1")).status, 404);
  assert.equal((await fetchPage(e, "/", { method: "POST", body: "x" })).status, 404);
  assert.equal(e.requests.length, 0);
  // A missing asset is a plain 404, without the asset server's body.
  const gone = env();
  gone.ASSETS.fetch = async () => new Response("detail", { status: 500 });
  const r = await fetchPage(gone, "/");
  assert.equal(r.status, 404);
  assert.equal(await r.text(), "");
  // Kill switch still stops everything, page included.
  const killed = env();
  killed.KILL_SWITCH = "true";
  assert.equal((await fetchPage(killed, "/")).status, 503);
  assert.equal(killed.requests.length, 0);
});

const phoneToken = "cd".repeat(32);
async function admit(h) {
  const p = h.pending();
  await h.send(p, admission("phone", phoneToken));
  return p;
}

test("refused phones learn why: home offline, not accepted, busy", async () => {
  const h = harness();
  // No bridge: home offline, whatever the token.
  let p = await admit(h);
  assert.equal(p.readyState, 3);
  assert.equal(p.code, PHONE_CLOSE.homeOffline);
  const b = h.pending();
  await h.send(b, admission("bridge", "ab".repeat(32)));
  // Bridge connected, token unknown or expired: not accepted.
  p = await admit(h);
  assert.equal(p.code, PHONE_CLOSE.notAccepted);
  await h.send(b, tokenSet(unhex(await hash(unhex(phoneToken))), 1, 7, 1n));
  p = await admit(h);
  assert.equal(p.code, PHONE_CLOSE.notAccepted);
  await h.send(b, tokenSet(unhex(await hash(unhex(phoneToken))), 1, 7, 0n));
  const admitted = [];
  for (let n = 0; n < 4; n++) admitted.push(await admit(h));
  assert.ok(admitted.every((s) => s.readyState === 1));
  p = await admit(h);
  assert.equal(p.code, PHONE_CLOSE.busy);
  // Malformed admission keeps the generic close.
  const bad = h.pending();
  await h.send(bad, new Uint8Array(34));
  assert.equal(bad.readyState, 3);
  assert.ok(!Object.values(PHONE_CLOSE).includes(bad.code));
});
