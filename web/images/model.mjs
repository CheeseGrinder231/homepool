// Pure helpers for the image page: no DOM, no network, testable in Node.

export const SETUP_FIELDS = ["k", "s", "b", "r"];

const PATTERNS = {
  accessKeyId: /^[A-Za-z0-9]{8,128}$/,
  secretAccessKey: /^[A-Za-z0-9+/=_-]{16,256}$/,
  bucket: /^[a-z0-9][a-z0-9-]{1,61}[a-z0-9]$/,
  region: /^[a-z0-9-]{1,32}$/,
};

// Accepts a setup link, its fragment, or the bare "k=..&s=..&b=..&r=.." text.
// Returns {accessKeyId, secretAccessKey, bucket, region} or throws a plain
// message. The values are validated so a mistyped or truncated paste fails
// here instead of as a confusing S3 error later.
export function parseSetup(text) {
  let fragment = String(text ?? "").trim();
  const hash = fragment.indexOf("#");
  if (hash >= 0) fragment = fragment.slice(hash + 1);
  const fields = new Map();
  for (const pair of fragment.split("&")) {
    if (pair === "") continue;
    const eq = pair.indexOf("=");
    if (eq < 0) throw new Error("The setup text is not a Homepool setup link.");
    let name;
    let value;
    try {
      name = decodeURIComponent(pair.slice(0, eq));
      value = decodeURIComponent(pair.slice(eq + 1));
    } catch {
      throw new Error("The setup text is not a Homepool setup link.");
    }
    fields.set(name, value);
  }
  for (const field of SETUP_FIELDS) {
    if (!fields.has(field)) throw new Error(`The setup link is incomplete: ${field} is missing.`);
  }
  const config = {
    accessKeyId: fields.get("k"),
    secretAccessKey: fields.get("s"),
    bucket: fields.get("b"),
    region: fields.get("r"),
  };
  for (const [name, pattern] of Object.entries(PATTERNS)) {
    if (!pattern.test(config[name])) throw new Error(`The setup link has an invalid ${name}.`);
  }
  return config;
}

export function setupFragment(config) {
  const pairs = [
    ["k", config.accessKeyId],
    ["s", config.secretAccessKey],
    ["b", config.bucket],
    ["r", config.region],
  ];
  return pairs.map(([name, value]) => `${name}=${encodeURIComponent(value)}`).join("&");
}

// Object keys only ever contain [A-Za-z0-9._-] segments separated by "/",
// never starting with a dot, so the Caddy site can refuse everything else.
export function safeName(fileName) {
  const base = String(fileName ?? "").split(/[\\/]/).pop() ?? "";
  const dot = base.lastIndexOf(".");
  const stem = dot > 0 ? base.slice(0, dot) : base;
  const extension = dot > 0 ? base.slice(dot + 1) : "";
  const clean = (part) => part.replace(/[^A-Za-z0-9._-]+/g, "-").replace(/^[.-]+|[.-]+$/g, "");
  const cleanStem = clean(stem).slice(0, 60) || "image";
  const cleanExtension = clean(extension).toLowerCase().slice(0, 12);
  return cleanExtension ? `${cleanStem}.${cleanExtension}` : cleanStem;
}

export function stamp(date) {
  const iso = date.toISOString();
  return { day: iso.slice(0, 10), time: iso.slice(11, 19).replace(/:/g, "") };
}

// "2026-11-02/143015-k7q2m9x4-IMG_0042.jpg": sorts newest first by key and is
// unique per upload because of the random part.
export function objectKey(fileName, date, random) {
  const { day, time } = stamp(date);
  return `${day}/${time}-${random}-${safeName(fileName)}`;
}

export function randomTag(bytes) {
  const alphabet = "abcdefghijklmnopqrstuvwxyz234567";
  let out = "";
  for (const byte of bytes) out += alphabet[byte % alphabet.length];
  return out;
}

export function displayName(key) {
  return key.split("/").pop() ?? key;
}

const IMAGE_EXTENSIONS = new Set([
  "jpg", "jpeg", "png", "gif", "webp", "avif", "bmp", "svg", "heic", "heif", "jxl", "ico", "tif", "tiff",
]);

export function extensionOf(key) {
  const name = displayName(key);
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(dot + 1).toLowerCase() : "";
}

export function looksLikeImage(key) {
  return IMAGE_EXTENSIONS.has(extensionOf(key));
}

export function formatBytes(size) {
  if (size < 1024) return `${size} B`;
  const units = ["KiB", "MiB", "GiB"];
  let value = size / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
}

const ENTITIES = { lt: "<", gt: ">", amp: "&", quot: '"', apos: "'" };

export function decodeXml(text) {
  return text.replace(/&(#x[0-9a-fA-F]+|#[0-9]+|[a-z]+);/g, (whole, entity) => {
    if (entity[0] === "#") {
      const code = entity[1] === "x" ? parseInt(entity.slice(2), 16) : parseInt(entity.slice(1), 10);
      return String.fromCodePoint(code);
    }
    return ENTITIES[entity] ?? whole;
  });
}

function element(xml, name) {
  const match = xml.match(new RegExp(`<${name}>([^<]*)</${name}>`));
  return match ? decodeXml(match[1]) : undefined;
}

// Parses a ListObjectsV2 response. Keys are returned as plain strings; the
// caller must render them as text, never as markup.
export function parseListing(xml) {
  if (!/<ListBucketResult[\s>]/.test(xml)) throw new Error("The listing response is not a bucket listing.");
  const objects = [];
  for (const [, contents] of xml.matchAll(/<Contents>([\s\S]*?)<\/Contents>/g)) {
    const key = element(contents, "Key");
    if (key === undefined) continue;
    objects.push({
      key,
      size: Number(element(contents, "Size") ?? 0),
      lastModified: element(contents, "LastModified") ?? "",
    });
  }
  return {
    objects,
    truncated: element(xml, "IsTruncated") === "true",
    continuationToken: element(xml, "NextContinuationToken"),
  };
}

export function sortNewestFirst(objects) {
  return [...objects].sort((a, b) => (a.key < b.key ? 1 : a.key > b.key ? -1 : 0));
}
