// AWS Signature Version 4 for S3-style services, using WebCrypto only.
// Works unchanged in browsers and in Node (`globalThis.crypto`).
//
// Path handling follows the S3 rule: the path is not normalized and each
// segment is URI-encoded exactly once. Pass decoded path segments; the
// signer encodes them. Query names and values are passed decoded as well.

const encoder = new TextEncoder();

export const UNSIGNED_PAYLOAD = "UNSIGNED-PAYLOAD";
export const EMPTY_SHA256 =
  "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

const ALGORITHM = "AWS4-HMAC-SHA256";

export function hex(bytes) {
  let out = "";
  for (const byte of new Uint8Array(bytes)) {
    out += byte.toString(16).padStart(2, "0");
  }
  return out;
}

function bytesOf(data) {
  if (typeof data === "string") return encoder.encode(data);
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  return data;
}

export async function sha256Hex(data) {
  return hex(await crypto.subtle.digest("SHA-256", bytesOf(data)));
}

async function hmac(key, message) {
  const cryptoKey = await crypto.subtle.importKey(
    "raw",
    bytesOf(key),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  return new Uint8Array(await crypto.subtle.sign("HMAC", cryptoKey, bytesOf(message)));
}

// AWS "UriEncode": unreserved characters stay, everything else becomes
// percent-encoded UTF-8 with uppercase hex digits.
export function uriEncode(text) {
  let out = "";
  for (const byte of encoder.encode(text)) {
    const char = String.fromCharCode(byte);
    if (/[A-Za-z0-9\-_.~]/.test(char)) {
      out += char;
    } else {
      out += "%" + byte.toString(16).toUpperCase().padStart(2, "0");
    }
  }
  return out;
}

// Encode a decoded path ("/bucket/a b/c") segment by segment, keeping
// empty segments so that "//x//" stays "//x//" (no normalization).
export function canonicalPath(path) {
  if (!path.startsWith("/")) throw new Error("path must start with /");
  return path.split("/").map(uriEncode).join("/");
}

// query: array of [name, value] pairs, decoded.
export function canonicalQuery(query) {
  const encoded = query.map(([name, value]) => [uriEncode(name), uriEncode(value ?? "")]);
  encoded.sort((a, b) => {
    if (a[0] !== b[0]) return a[0] < b[0] ? -1 : 1;
    if (a[1] === b[1]) return 0;
    return a[1] < b[1] ? -1 : 1;
  });
  return encoded.map(([name, value]) => `${name}=${value}`).join("&");
}

// headers: array of [name, value] pairs or a plain object. Names are
// lowercased, values trimmed with internal whitespace collapsed, and
// repeated names are joined with commas in order of appearance.
export function canonicalHeaders(headers) {
  const pairs = Array.isArray(headers) ? headers : Object.entries(headers);
  const merged = new Map();
  for (const [name, value] of pairs) {
    const lower = name.trim().toLowerCase();
    const clean = String(value).trim().replace(/\s+/g, " ");
    merged.set(lower, merged.has(lower) ? `${merged.get(lower)},${clean}` : clean);
  }
  const names = [...merged.keys()].sort();
  return {
    signedHeaders: names.join(";"),
    canonical: names.map((name) => `${name}:${merged.get(name)}\n`).join(""),
  };
}

export function amzDate(date) {
  return date.toISOString().replace(/[-:]/g, "").replace(/\.\d{3}/, "");
}

function scopeOf(dayStamp, credentials) {
  return `${dayStamp}/${credentials.region}/${credentials.service ?? "s3"}/aws4_request`;
}

async function signingKey(credentials, dayStamp) {
  let key = await hmac(`AWS4${credentials.secretAccessKey}`, dayStamp);
  for (const part of [credentials.region, credentials.service ?? "s3", "aws4_request"]) {
    key = await hmac(key, part);
  }
  return key;
}

export function canonicalRequest({ method, path, query, headers, payloadHash }) {
  const { signedHeaders, canonical } = canonicalHeaders(headers);
  return {
    signedHeaders,
    text: [
      method.toUpperCase(),
      canonicalPath(path),
      canonicalQuery(query ?? []),
      canonical,
      signedHeaders,
      payloadHash,
    ].join("\n"),
  };
}

async function signature(credentials, date, requestText) {
  const stamp = amzDate(date);
  const dayStamp = stamp.slice(0, 8);
  const scope = scopeOf(dayStamp, credentials);
  const stringToSign = [ALGORITHM, stamp, scope, await sha256Hex(requestText)].join("\n");
  const key = await signingKey(credentials, dayStamp);
  return { scope, stringToSign, signature: hex(await hmac(key, stringToSign)) };
}

// Sign with the Authorization header. `headers` are the request headers the
// caller will send (without host); all of them are signed. Returns the full
// header set to send, including x-amz-date and Authorization.
export async function signHeaders(request, credentials, date = new Date()) {
  const stamp = amzDate(date);
  const headers = [
    ["host", request.host],
    ...(Array.isArray(request.headers) ? request.headers : Object.entries(request.headers ?? {})),
    ["x-amz-date", stamp],
  ];
  if (request.contentSha256 !== undefined) {
    headers.push(["x-amz-content-sha256", request.contentSha256]);
  }
  if (credentials.sessionToken) headers.push(["x-amz-security-token", credentials.sessionToken]);
  const payloadHash = request.payloadHash ?? request.contentSha256 ?? EMPTY_SHA256;
  const canonical = canonicalRequest({ ...request, headers, payloadHash });
  const signed = await signature(credentials, date, canonical.text);
  const authorization =
    `${ALGORITHM} Credential=${credentials.accessKeyId}/${signed.scope}, ` +
    `SignedHeaders=${canonical.signedHeaders}, Signature=${signed.signature}`;
  const out = {};
  for (const [name, value] of headers) {
    if (name !== "host") out[name] = value;
  }
  out.authorization = authorization;
  return { headers: out, canonicalRequest: canonical.text, stringToSign: signed.stringToSign, signature: signed.signature };
}

// Presign: returns the query pairs (including X-Amz-Signature) to append.
// Only the host header is signed, as the AWS test suite and browsers expect.
export async function presignQuery(request, credentials, date = new Date(), expires = 300) {
  const stamp = amzDate(date);
  const dayStamp = stamp.slice(0, 8);
  const headers = [
    ["host", request.host],
    ...(Array.isArray(request.headers) ? request.headers : Object.entries(request.headers ?? {})),
  ];
  const { signedHeaders } = canonicalHeaders(headers);
  const query = [
    ...(request.query ?? []),
    ["X-Amz-Algorithm", ALGORITHM],
    ["X-Amz-Credential", `${credentials.accessKeyId}/${scopeOf(dayStamp, credentials)}`],
    ["X-Amz-Date", stamp],
    ["X-Amz-Expires", String(expires)],
    ["X-Amz-SignedHeaders", signedHeaders],
  ];
  if (credentials.sessionToken) query.push(["X-Amz-Security-Token", credentials.sessionToken]);
  const payloadHash = request.payloadHash ?? UNSIGNED_PAYLOAD;
  const canonical = canonicalRequest({ ...request, query, headers, payloadHash });
  const signed = await signature(credentials, date, canonical.text);
  query.push(["X-Amz-Signature", signed.signature]);
  return { query, canonicalRequest: canonical.text, stringToSign: signed.stringToSign, signature: signed.signature };
}

// Convenience for browsers: a complete presigned URL for a GET.
export async function presignUrl(origin, host, path, credentials, expires, date = new Date()) {
  const { query } = await presignQuery({ method: "GET", host, path, query: [] }, credentials, date, expires);
  const search = query.map(([name, value]) => `${uriEncode(name)}=${uriEncode(value)}`).join("&");
  return `${origin}${canonicalPath(path)}?${search}`;
}
