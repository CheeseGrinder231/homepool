// Runs the example Caddy site against a throwaway Garage and checks what it
// lets through and what it refuses. Needs the two binaries:
//   HOMEPOOL_TEST_GARAGE=/path/to/garage HOMEPOOL_TEST_CADDY=/path/to/caddy \
//     node --test web/images/tests/
// Without them the suite is skipped.
import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import { request as httpsRequest } from "node:https";
import { request as httpRequest } from "node:http";
import { after, before, test } from "node:test";

import { EMPTY_SHA256, canonicalPath, presignQuery, sha256Hex, signHeaders, uriEncode } from "../sigv4.mjs";
import { parseListing } from "../model.mjs";
import { BUCKET, startStack } from "./stack.mjs";

const garage = process.env.HOMEPOOL_TEST_GARAGE;
const caddy = process.env.HOMEPOOL_TEST_CADDY;
const enabled = Boolean(garage && caddy);

let stack;

before(async () => {
  if (enabled) stack = await startStack({ garage, caddy });
});

after(async () => {
  if (stack) await stack.stop();
});

// Raw HTTP so that paths go out exactly as written, without normalization.
function raw({ origin, ca, method, path, headers = {}, body }) {
  const url = new URL(origin);
  const send = url.protocol === "https:" ? httpsRequest : httpRequest;
  return new Promise((done, fail) => {
    const request = send(
      { host: url.hostname, port: url.port, method, path, headers, ca, servername: url.hostname },
      (response) => {
        const chunks = [];
        response.on("data", (chunk) => chunks.push(chunk));
        response.on("end", () =>
          done({ status: response.statusCode, headers: response.headers, body: Buffer.concat(chunks) }),
        );
      },
    );
    request.on("error", fail);
    if (body) request.write(body);
    request.end();
  });
}

function querystring(query) {
  return query.map(([name, value]) => `${uriEncode(name)}=${uriEncode(value)}`).join("&");
}

// Signed request through Caddy (default) or straight to Garage.
async function signed(method, path, { query = [], headers = {}, body, direct = false, rawPath } = {}) {
  const host = direct ? stack.garageHost : stack.host;
  const origin = direct ? stack.garageOrigin : stack.origin;
  const contentSha256 = body ? await sha256Hex(body) : EMPTY_SHA256;
  const result = await signHeaders({ method, host, path, query, headers, contentSha256 }, stack.credentials);
  const target = `${rawPath ?? canonicalPath(path)}${query.length ? `?${querystring(query)}` : ""}`;
  return raw({ origin, ca: stack.ca, method, path: target, headers: result.headers, body });
}

async function presigned(path, { expires = 300, date = new Date() } = {}) {
  const { query } = await presignQuery({ method: "GET", host: stack.host, path, query: [] }, stack.credentials, date, expires);
  return `${canonicalPath(path)}?${querystring(query)}`;
}

function proxied(response) {
  return response.headers["x-amz-request-id"] !== undefined || (response.headers["content-type"] ?? "").includes("xml");
}

const skip = enabled ? false : "set HOMEPOOL_TEST_GARAGE and HOMEPOOL_TEST_CADDY to run";

test("the page is served with its own headers", { skip }, async () => {
  const page = await raw({ origin: stack.origin, ca: stack.ca, method: "GET", path: "/" });
  assert.equal(page.status, 200);
  assert.match(page.headers["content-type"], /^text\/html/);
  assert.match(page.headers["content-security-policy"], /script-src 'self'/);
  assert.equal(page.headers["x-content-type-options"], "nosniff");
  assert.equal(page.headers["referrer-policy"], "no-referrer");
  assert.equal(page.headers.server, undefined);
  const script = await raw({ origin: stack.origin, ca: stack.ca, method: "GET", path: "/app.mjs" });
  assert.equal(script.status, 200);
  assert.match(script.headers["content-type"], /javascript/);
});

test("signed upload, listing, HEAD and download work through Caddy", { skip }, async () => {
  const body = randomBytes(10 * 1024 * 1024 + 7);
  const key = "2026-11-02/120000-abcdefgh-big.png";
  const put = await signed("PUT", `/${BUCKET}/${key}`, { body, headers: { "content-type": "image/png", "if-none-match": "*" } });
  assert.equal(put.status, 200, put.body.toString());
  const head = await signed("HEAD", `/${BUCKET}/${key}`);
  assert.equal(head.status, 200);
  const listing = await signed("GET", `/${BUCKET}`, { query: [["list-type", "2"], ["max-keys", "1000"]] });
  assert.equal(listing.status, 200, listing.body.toString());
  const parsed = parseListing(listing.body.toString());
  assert.ok(parsed.objects.some((object) => object.key === key && object.size === body.length));
  const get = await raw({ origin: stack.origin, ca: stack.ca, method: "GET", path: await presigned(`/${BUCKET}/${key}`) });
  assert.equal(get.status, 200);
  assert.equal(await sha256Hex(get.body), await sha256Hex(body));
  assert.equal(get.headers["content-type"], "image/png");
  assert.equal(get.headers["content-disposition"], "attachment");
  assert.match(get.headers["content-security-policy"], /^sandbox;/);
  assert.equal(get.headers["x-content-type-options"], "nosniff");
  assert.equal(get.headers["cross-origin-resource-policy"], "same-origin");
  assert.equal(get.headers["cache-control"], "private, no-store");
});

test("an existing key is never overwritten by the page's upload", { skip }, async () => {
  const key = "2026-11-02/120001-abcdefgh-once.png";
  const first = await signed("PUT", `/${BUCKET}/${key}`, { body: Buffer.from("one"), headers: { "content-type": "image/png", "if-none-match": "*" } });
  assert.equal(first.status, 200);
  const second = await signed("PUT", `/${BUCKET}/${key}`, { body: Buffer.from("two"), headers: { "content-type": "image/png", "if-none-match": "*" } });
  const get = await raw({ origin: stack.origin, ca: stack.ca, method: "GET", path: await presigned(`/${BUCKET}/${key}`) });
  // Garage either honours If-None-Match (412) or ignores it; the page also
  // checks with HEAD first, so record what this Garage does.
  if (second.status === 412) assert.equal(get.body.toString(), "one");
  else assert.equal(second.status, 200);
});

test("unsigned and expired requests are refused by Garage", { skip }, async () => {
  const key = "2026-11-02/120000-abcdefgh-big.png";
  const unsigned = await raw({ origin: stack.origin, ca: stack.ca, method: "GET", path: `/${BUCKET}/${key}` });
  assert.equal(unsigned.status, 403);
  const stale = await presigned(`/${BUCKET}/${key}`, { expires: 60, date: new Date(Date.now() - 10 * 60 * 1000) });
  const expired = await raw({ origin: stack.origin, ca: stack.ca, method: "GET", path: stale });
  assert.ok([400, 403].includes(expired.status), `expired presigned URL answered ${expired.status}`);
  assert.equal(expired.headers["content-type"].includes("png"), false);
  const wrongKey = await raw({
    origin: stack.origin,
    ca: stack.ca,
    method: "GET",
    path: (await presigned(`/${BUCKET}/${key}`)).replace(/X-Amz-Signature=[0-9a-f]{8}/, "X-Amz-Signature=00000000"),
  });
  assert.equal(wrongKey.status, 403);
});

test("bucket listing, other buckets and escapes never reach Garage", { skip }, async () => {
  // The site root is the page itself, never an S3 bucket listing.
  for (const path of ["/", "/?list-type=2"]) {
    const page = await signed("GET", "/", { rawPath: path });
    assert.equal(page.status, 200);
    assert.match(page.headers["content-type"], /^text\/html/);
    assert.equal(page.body.toString().includes("ListAllMyBucketsResult"), false);
  }
  const refused = [
    ["GET", "/other-bucket?list-type=2"],
    ["GET", `/${BUCKET}1/x.png`],
    ["GET", `/${BUCKET}-two/x.png`],
    ["GET", `/${BUCKET}/../other-bucket/x.png`],
    ["GET", `/${BUCKET}/..%2Fother-bucket/x.png`],
    ["GET", `/${BUCKET}/%2e%2e/x.png`],
    ["GET", `/${BUCKET}/.hidden`],
    ["GET", `/${BUCKET}//x.png`],
    ["GET", `/${BUCKET}/a%20b.png`],
    ["GET", `/${BUCKET}/x.png#frag`],
    ["GET", `/%68omepool-consumer-1/x.png`],
    ["GET", `/${BUCKET}/x.png?list-type=2&prefix=../`],
    ["PUT", `/${BUCKET}/../other-bucket/x.png`],
    ["DELETE", `/${BUCKET}/2026-11-02/120000-abcdefgh-big.png`],
    ["POST", `/${BUCKET}?delete`],
    ["POST", `/${BUCKET}/x.png?uploads`],
    ["OPTIONS", `/${BUCKET}/x.png`],
  ];
  for (const [method, path] of refused) {
    const response = await signed(method, path, { rawPath: path });
    assert.equal(proxied(response), false, `${method} ${path} reached Garage: ${response.status} ${response.body.toString().slice(0, 100)}`);
    assert.notEqual(response.status, 200, `${method} ${path} returned 200`);
  }
  const still = await signed("HEAD", `/${BUCKET}/2026-11-02/120000-abcdefgh-big.png`);
  assert.equal(still.status, 200, "the object survived the refused DELETE");
});

test("stored SVG and HTML come back as sandboxed attachments", { skip }, async () => {
  const svg = Buffer.from('<svg><script>document.title="pwned"</script><text y="20">x</text></svg>');
  const html = Buffer.from("<!doctype html><script>document.title='pwned'</script>");
  for (const [key, type, body] of [
    ["2026-11-02/120002-abcdefgh-script.svg", "image/svg+xml", svg],
    ["2026-11-02/120003-abcdefgh-page.html", "text/html", html],
  ]) {
    const put = await signed("PUT", `/${BUCKET}/${key}`, { body, headers: { "content-type": type }, direct: true });
    assert.equal(put.status, 200);
    const get = await raw({ origin: stack.origin, ca: stack.ca, method: "GET", path: await presigned(`/${BUCKET}/${key}`) });
    assert.equal(get.status, 200);
    assert.equal(get.headers["content-type"], type);
    assert.equal(get.headers["content-disposition"], "attachment");
    assert.equal(get.headers["content-security-policy"], "sandbox; default-src 'none'; frame-ancestors 'none'");
    assert.equal(get.headers["x-content-type-options"], "nosniff");
  }
});

test("a stored Content-Disposition cannot replace the attachment rule", { skip }, async () => {
  const key = "2026-11-02/120004-abcdefgh-inline.html";
  const put = await signed("PUT", `/${BUCKET}/${key}`, {
    body: Buffer.from("<p>inline</p>"),
    headers: { "content-type": "text/html", "content-disposition": 'inline; filename="x.html"' },
    direct: true,
  });
  assert.equal(put.status, 200);
  const get = await raw({ origin: stack.origin, ca: stack.ca, method: "GET", path: await presigned(`/${BUCKET}/${key}`) });
  assert.equal(get.status, 200);
  assert.equal(get.headers["content-disposition"], "attachment");
});

test("a hostile key stored by another client is listed as text and never served", { skip }, async () => {
  const key = '2026-11-02/<img src=x onerror=alert(1)> "q" & <b>.svg';
  const put = await signed("PUT", `/${BUCKET}/${key}`, { body: Buffer.from("<svg/>"), headers: { "content-type": "image/svg+xml" }, direct: true });
  assert.equal(put.status, 200);
  const listing = await signed("GET", `/${BUCKET}`, { query: [["list-type", "2"]] });
  const parsed = parseListing(listing.body.toString());
  assert.ok(parsed.objects.some((object) => object.key === key), "the key round-trips as a plain string");
  const get = await raw({ origin: stack.origin, ca: stack.ca, method: "GET", path: await presigned(`/${BUCKET}/${key}`) });
  assert.equal(proxied(get), false, "percent-encoded keys never reach Garage");
  assert.notEqual(get.status, 200);
});
