// Checks sigv4.mjs against AWS's published Signature Version 4 vectors.
// Run with: node --test web/images/tests/
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { presignQuery, sha256Hex, signHeaders } from "../sigv4.mjs";

const vectors = JSON.parse(readFileSync(join(dirname(fileURLToPath(import.meta.url)), "vectors.json"), "utf8"));

function parseRequest(text) {
  const [head, ...bodyParts] = text.split("\n\n");
  const lines = head.split("\n");
  const requestLine = lines[0].replace(/ HTTP\/1\.1$/, "");
  const method = requestLine.slice(0, requestLine.indexOf(" "));
  const target = requestLine.slice(method.length + 1);
  const mark = target.indexOf("?");
  const rawPath = mark < 0 ? target : target.slice(0, mark);
  const rawQuery = mark < 0 ? "" : target.slice(mark + 1);
  const path = rawPath.split("/").map(decodeURIComponent).join("/");
  const query = rawQuery
    ? rawQuery.split("&").map((pair) => {
        const eq = pair.indexOf("=");
        return eq < 0
          ? [decodeURIComponent(pair), ""]
          : [decodeURIComponent(pair.slice(0, eq)), decodeURIComponent(pair.slice(eq + 1))];
      })
    : [];
  const headers = [];
  for (const line of lines.slice(1).filter((entry) => entry !== "")) {
    if (/^\s/.test(line)) {
      headers[headers.length - 1][1] += `\n${line}`;
    } else {
      const colon = line.indexOf(":");
      headers.push([line.slice(0, colon), line.slice(colon + 1)]);
    }
  }
  const hostIndex = headers.findIndex(([name]) => name.toLowerCase() === "host");
  const host = headers[hostIndex][1];
  headers.splice(hostIndex, 1);
  return { method, path, query, headers, host, body: bodyParts.join("\n\n") };
}

for (const [name, vector] of Object.entries(vectors)) {
  const context = vector.context;
  const request = parseRequest(vector.request);
  const credentials = {
    accessKeyId: context.credentials.access_key_id,
    secretAccessKey: context.credentials.secret_access_key,
    // omit_session_token means the token travels unsigned, added after signing.
    sessionToken: context.omit_session_token ? undefined : context.credentials.token,
    region: context.region,
    service: context.service,
  };
  const date = new Date(context.timestamp);

  test(`${name}: Authorization header`, async () => {
    const payloadHash = await sha256Hex(request.body);
    const signed = await signHeaders(
      {
        method: request.method,
        host: request.host,
        path: request.path,
        query: request.query,
        headers: request.headers,
        payloadHash,
        contentSha256: context.sign_body ? payloadHash : undefined,
      },
      credentials,
      date,
    );
    assert.equal(signed.canonicalRequest, vector.headerCanonicalRequest);
    assert.equal(signed.signature, vector.headerSignature);
    assert.match(signed.headers.authorization, new RegExp(`Signature=${signed.signature}$`));
  });

  test(`${name}: presigned query`, async () => {
    const signed = await presignQuery(
      {
        method: request.method,
        host: request.host,
        path: request.path,
        query: request.query,
        headers: request.headers,
        payloadHash: await sha256Hex(request.body),
      },
      credentials,
      date,
      context.expiration_in_seconds,
    );
    assert.equal(signed.canonicalRequest, vector.queryCanonicalRequest);
    assert.equal(signed.signature, vector.querySignature);
  });
}
