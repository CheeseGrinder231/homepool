// Runs the example Caddy site against a throwaway Garage and checks what it
// lets through and what it refuses. Needs the two binaries:
//   HOMEPOOL_TEST_GARAGE=/path/to/garage HOMEPOOL_TEST_CADDY=/path/to/caddy \
//     node --test web/images/tests/site.test.mjs
// Without them the suite is skipped.
import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import { request as httpsRequest } from "node:https";
import { request as httpRequest } from "node:http";
import { after, before, test } from "node:test";

import { EMPTY_SHA256, canonicalPath, presignQuery, sha256Hex, signHeaders, uriEncode } from "../sigv4.mjs";
import { RANDOM_TAG_BYTES, objectKey, parseListing, randomTag } from "../model.mjs";
import { BUCKET, startStack } from "./stack.mjs";

const garage = process.env.HOMEPOOL_TEST_GARAGE;
const caddy = process.env.HOMEPOOL_TEST_CADDY;
const enabled = Boolean(garage && caddy);
const skip = enabled ? false : "set HOMEPOOL_TEST_GARAGE and HOMEPOOL_TEST_CADDY to run";

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

// The exact presigned shape the page builds.
async function presigned(path, { expires = 300, date = new Date() } = {}) {
  const { query } = await presignQuery({ method: "GET", host: stack.host, path, query: [] }, stack.credentials, date, expires);
  return `${canonicalPath(path)}?${querystring(query)}`;
}

function viaCaddy(method, path, options = {}) {
  return raw({ origin: stack.origin, ca: stack.ca, method, path, ...options });
}

function proxied(response) {
  return response.headers["x-amz-request-id"] !== undefined || (response.headers["content-type"] ?? "").includes("xml");
}

function pageKey(name, date = new Date()) {
  return objectKey(name, date, randomTag(randomBytes(RANDOM_TAG_BYTES)));
}

const LIST = [["list-type", "2"], ["max-keys", "1000"]];

// Follows Garage's continuation tokens through Caddy, as the page does.
async function listAllKeys() {
  const keys = [];
  let token;
  let pages = 0;
  do {
    const query = token ? [...LIST, ["continuation-token", token]] : LIST;
    const response = await signed("GET", `/${BUCKET}`, { query });
    assert.equal(response.status, 200, response.body.toString().slice(0, 200));
    const page = parseListing(response.body.toString());
    keys.push(...page.objects.map((object) => object.key));
    token = page.truncated ? page.continuationToken : undefined;
    pages += 1;
  } while (token);
  return { keys, pages };
}

test("the page is served with its own headers", { skip }, async () => {
  const page = await viaCaddy("GET", "/");
  assert.equal(page.status, 200);
  assert.match(page.headers["content-type"], /^text\/html/);
  assert.match(page.headers["content-security-policy"], /script-src 'self'/);
  assert.equal(page.headers["x-content-type-options"], "nosniff");
  assert.equal(page.headers["referrer-policy"], "no-referrer");
  assert.equal(page.headers.server, undefined);
  const script = await viaCaddy("GET", "/app.mjs");
  assert.equal(script.status, 200);
  assert.match(script.headers["content-type"], /javascript/);
  // The detector used below must recognise a Garage answer.
  const fromGarage = await raw({ origin: stack.garageOrigin, method: "GET", path: `/${BUCKET}/missing.png` });
  assert.equal(proxied(fromGarage), true, JSON.stringify(fromGarage.headers));
});

const bigKey = pageKey("big.png");

test("the page's upload, listing and download shapes work through Caddy", { skip }, async () => {
  const body = randomBytes(10 * 1024 * 1024 + 7);
  const put = await signed("PUT", `/${BUCKET}/${bigKey}`, { body, headers: { "content-type": "image/png" } });
  assert.equal(put.status, 200, put.body.toString());
  const listing = await signed("GET", `/${BUCKET}`, { query: LIST });
  assert.equal(listing.status, 200, listing.body.toString());
  const parsed = parseListing(listing.body.toString());
  assert.ok(parsed.objects.some((object) => object.key === bigKey && object.size === body.length));
  const get = await viaCaddy("GET", await presigned(`/${BUCKET}/${bigKey}`));
  assert.equal(get.status, 200);
  assert.equal(await sha256Hex(get.body), await sha256Hex(body));
  assert.equal(get.headers["content-type"], "image/png");
  assert.equal(get.headers["content-disposition"], "attachment");
  assert.match(get.headers["content-security-policy"], /^sandbox;/);
  assert.equal(get.headers["x-content-type-options"], "nosniff");
  assert.equal(get.headers["cross-origin-resource-policy"], "same-origin");
  assert.equal(get.headers["cache-control"], "private, no-store");
});

test("listing pages through Caddy with Garage's continuation tokens", { skip }, async () => {
  // Garage's token is "[" or "]" plus standard base64 of a key; 1002 objects
  // need two pages at the page's fixed size of 1000.
  const date = new Date("2020-01-01T00:00:00Z");
  const keys = Array.from({ length: 1002 }, (_, i) => pageKey(`p${i}.png`, date));
  for (let i = 0; i < keys.length; i += 50) {
    await Promise.all(
      keys.slice(i, i + 50).map((key) =>
        signed("PUT", `/${BUCKET}/${key}`, { body: Buffer.from("x"), headers: { "content-type": "image/png" }, direct: true }),
      ),
    );
  }
  const { keys: seen, pages } = await listAllKeys();
  assert.ok(pages >= 2);
  for (const key of keys) assert.ok(seen.includes(key), `${key} listed`);
});

test("unsigned, expired and tampered presigned requests are refused by Garage", { skip }, async () => {
  const unsigned = await signed("GET", `/${BUCKET}/${bigKey}`, { rawPath: `/${BUCKET}/${bigKey}` });
  assert.equal(proxied(unsigned), false, "an object GET without the presigned query never reaches Garage");
  const stale = await presigned(`/${BUCKET}/${bigKey}`, { expires: 60, date: new Date(Date.now() - 10 * 60 * 1000) });
  const expired = await viaCaddy("GET", stale);
  assert.ok([400, 403].includes(expired.status), `expired presigned URL answered ${expired.status}`);
  assert.equal(expired.headers["content-type"].includes("png"), false);
  const tampered = (await presigned(`/${BUCKET}/${bigKey}`)).replace(/X-Amz-Signature=[0-9a-f]{8}/, "X-Amz-Signature=00000000");
  assert.equal((await viaCaddy("GET", tampered)).status, 403);
});

// A lifecycle rule that Garage accepts from this key when asked directly.
const lifecycle = Buffer.from(
  '<LifecycleConfiguration><Rule><ID>expire</ID><Status>Enabled</Status><Filter><Prefix></Prefix></Filter><Expiration><Days>1</Days></Expiration></Rule></LifecycleConfiguration>',
);

test("bucket configuration and copies are refused before Garage, with bodies Garage would accept", { skip }, async () => {
  const direct = await signed("PUT", `/${BUCKET}`, { query: [["lifecycle", ""]], body: lifecycle, headers: { "content-type": "application/xml" }, direct: true });
  assert.equal(direct.status, 200, `Garage accepts the lifecycle body from this key: ${direct.body}`);
  const reset = await signed("DELETE", `/${BUCKET}`, { query: [["lifecycle", ""]], direct: true });
  assert.ok([200, 204].includes(reset.status), "lifecycle removed again directly");

  const copyKey = pageKey("copy.png");
  const directCopy = await signed("PUT", `/${BUCKET}/${copyKey}`, { headers: { "x-amz-copy-source": `/${BUCKET}/${bigKey}` }, direct: true });
  assert.equal(directCopy.status, 200, "Garage accepts CopyObject from this key");

  const refused = [
    ["PUT", `/${BUCKET}?lifecycle`, { body: lifecycle, headers: { "content-type": "application/xml" } }],
    ["GET", `/${BUCKET}?lifecycle`, {}],
    ["DELETE", `/${BUCKET}?lifecycle`, {}],
    ["PUT", `/${BUCKET}/${pageKey("c.png")}`, { headers: { "x-amz-copy-source": `/${BUCKET}/${bigKey}` } }],
    ["PUT", `/${BUCKET}/${pageKey("t.png")}?tagging`, { body: Buffer.from("<Tagging/>") }],
    ["PUT", `/${BUCKET}/${pageKey("a.png")}?acl`, {}],
    ["PUT", `/${BUCKET}/${pageKey("m.png")}?uploadId=x&partNumber=1`, { body: Buffer.from("x") }],
    ["POST", `/${BUCKET}/${pageKey("m.png")}?uploads`, {}],
    ["POST", `/${BUCKET}?delete`, { body: Buffer.from("<Delete/>") }],
    ["DELETE", `/${BUCKET}/${bigKey}`, {}],
    ["HEAD", `/${BUCKET}/${bigKey}`, {}],
    ["GET", `/${BUCKET}?uploads`, {}],
    ["GET", `/${BUCKET}?versioning`, {}],
    ["GET", `/${BUCKET}?list-type=2&max-keys=1000&prefix=2026`, {}],
    ["GET", `/${BUCKET}?list-type=2&max-keys=5`, {}],
    ["GET", `/${BUCKET}?list-type=2`, {}],
    ["GET", `/${BUCKET}`, {}],
    ["GET", `/${BUCKET}?list-type=2&max-keys=1000&continuation-token=plain`, {}],
    ["PUT", `/${BUCKET}/${pageKey("x.png")}?x-id=PutObject`, { body: Buffer.from("x") }],
    ["PUT", `/${BUCKET}/plain-name.png`, { body: Buffer.from("x") }],
    ["PUT", `/${BUCKET}/2026-11-02/120000-short-name.png`, { body: Buffer.from("x") }],
    ["PUT", `/${BUCKET}/2026-11-02/120000-${"a".repeat(26)}-.hidden`, { body: Buffer.from("x") }],
    ["PUT", `/${BUCKET}`, { body: Buffer.from("x") }],
  ];
  for (const [method, target, options] of refused) {
    const path = target.split("?")[0];
    const response = await signed(method, path, { ...options, rawPath: target });
    assert.equal(proxied(response), false, `${method} ${target} reached Garage: ${response.status} ${response.body.toString().slice(0, 100)}`);
    assert.notEqual(response.status, 200, `${method} ${target} returned 200`);
  }
  const lifecycleNow = await signed("GET", `/${BUCKET}`, { query: [["lifecycle", ""]], direct: true });
  assert.equal(lifecycleNow.status, 404, "no lifecycle rule reached Garage through Caddy");
  const still = await signed("HEAD", `/${BUCKET}/${bigKey}`, { direct: true });
  assert.equal(still.status, 200, "the object survived the refused DELETE");
});

test("a presigned GET with anything added or missing is refused before Garage", { skip }, async () => {
  const good = await presigned(`/${BUCKET}/${bigKey}`);
  for (const path of [
    `${good}&response-content-type=text%2Fhtml`,
    good.replace("&X-Amz-SignedHeaders=host", "&X-Amz-SignedHeaders=host%3Bx-amz-date"),
    good.replace("X-Amz-Expires=300", "X-Amz-Expires=604800"),
    good.replace("?X-Amz-Algorithm", "?versionId=1&X-Amz-Algorithm"),
    `/${BUCKET}/${bigKey}?list-type=2&max-keys=1000`,
  ]) {
    const response = await viaCaddy("GET", path);
    assert.equal(proxied(response), false, `${path.slice(0, 80)} reached Garage`);
    assert.notEqual(response.status, 200);
  }
});

test("bucket listing, other buckets and escapes never reach Garage", { skip }, async () => {
  // The site root is the page itself, never an S3 bucket listing.
  for (const path of ["/", "/?list-type=2&max-keys=1000"]) {
    const page = await signed("GET", "/", { rawPath: path });
    assert.equal(page.status, 200);
    assert.match(page.headers["content-type"], /^text\/html/);
    assert.equal(page.body.toString().includes("ListAllMyBucketsResult"), false);
  }
  const refused = [
    ["GET", "/other-bucket?list-type=2&max-keys=1000"],
    ["GET", `/${BUCKET}1?list-type=2&max-keys=1000`],
    ["GET", `/${BUCKET}-two?list-type=2&max-keys=1000`],
    ["GET", `/${BUCKET}/../other-bucket?list-type=2&max-keys=1000`],
    ["GET", `/${BUCKET}/..%2Fother-bucket/x.png`],
    ["GET", `/${BUCKET}/%2e%2e/x.png`],
    ["GET", `/${BUCKET}//x.png`],
    ["GET", `/%68omepool-consumer-1?list-type=2&max-keys=1000`],
    ["PUT", `/${BUCKET}/../other-bucket/${pageKey("x.png").split("/")[1]}`],
    ["OPTIONS", `/${BUCKET}/x.png`],
  ];
  for (const [method, path] of refused) {
    const response = await signed(method, path, { rawPath: path });
    assert.equal(proxied(response), false, `${method} ${path} reached Garage: ${response.status} ${response.body.toString().slice(0, 100)}`);
    assert.notEqual(response.status, 200, `${method} ${path} returned 200`);
  }
});

test("stored SVG and HTML come back as sandboxed attachments", { skip }, async () => {
  const svg = Buffer.from('<svg><script>document.title="pwned"</script><text y="20">x</text></svg>');
  const html = Buffer.from("<!doctype html><script>document.title='pwned'</script>");
  for (const [name, type, body] of [
    ["script.svg", "image/svg+xml", svg],
    ["page.html", "text/html", html],
  ]) {
    const key = pageKey(name);
    const put = await signed("PUT", `/${BUCKET}/${key}`, { body, headers: { "content-type": type }, direct: true });
    assert.equal(put.status, 200);
    const get = await viaCaddy("GET", await presigned(`/${BUCKET}/${key}`));
    assert.equal(get.status, 200);
    assert.equal(get.headers["content-type"], type);
    assert.equal(get.headers["content-disposition"], "attachment");
    assert.equal(get.headers["content-security-policy"], "sandbox; default-src 'none'; frame-ancestors 'none'");
    assert.equal(get.headers["x-content-type-options"], "nosniff");
  }
});

test("a stored Content-Disposition cannot replace the attachment rule", { skip }, async () => {
  const key = pageKey("inline.html");
  const put = await signed("PUT", `/${BUCKET}/${key}`, {
    body: Buffer.from("<p>inline</p>"),
    headers: { "content-type": "text/html", "content-disposition": 'inline; filename="x.html"' },
    direct: true,
  });
  assert.equal(put.status, 200);
  const get = await viaCaddy("GET", await presigned(`/${BUCKET}/${key}`));
  assert.equal(get.status, 200);
  assert.equal(get.headers["content-disposition"], "attachment");
});

test("a hostile key stored by another client is listed as text and never served", { skip }, async () => {
  const key = '2026-11-02/<img src=x onerror=alert(1)> "q" & <b>.svg';
  const put = await signed("PUT", `/${BUCKET}/${key}`, { body: Buffer.from("<svg/>"), headers: { "content-type": "image/svg+xml" }, direct: true });
  assert.equal(put.status, 200);
  const { keys } = await listAllKeys();
  assert.ok(keys.includes(key), "the key round-trips as a plain string");
  const get = await viaCaddy("GET", await presigned(`/${BUCKET}/${key}`));
  assert.equal(proxied(get), false, "percent-encoded keys never reach Garage");
  assert.notEqual(get.status, 200);
});
