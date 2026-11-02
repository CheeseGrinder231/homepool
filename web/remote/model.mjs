// Pure helpers for the remote image page: no DOM, no network, testable in Node.
// Strings from links, files and the network are data; the page assigns them to
// textContent only.

const enc = new TextEncoder();
const FIELDS = ["route", "token", "bridge", "invitation", "psk"];
const PATTERNS = {
  route: /^[A-Za-z0-9_-]{1,64}$/,
  token: /^[0-9a-f]{64}$/,
  bridge: /^[0-9a-f]{64}$/,
  invitation: /^[0-9a-f]{32}$/,
  psk: /^[0-9a-f]{64}$/,
};

// A link the page refuses for its shape. Everything else about a pairing code
// (expired, used, another home's key) is only learned by trying it.
export class LinkError extends Error {
  constructor(reason) {
    super(reason);
    this.reason = reason;
  }
}

// Accepts the pairing link or its fragment. `origin` is this page's origin: a
// pasted link for another address is refused rather than used against this relay.
// Exactly the five fields, once each, in canonical lowercase hex; no extras.
export function parsePairing(text, origin) {
  const input = String(text ?? "").trim();
  if (!input || input.length > 1024) throw new LinkError("malformed");
  const hash = input.indexOf("#");
  if (hash > 0) {
    let url;
    try {
      url = new URL(input);
    } catch {
      throw new LinkError("malformed");
    }
    if (origin && url.origin !== origin) throw new LinkError("other-origin");
    if (url.pathname !== "/" || url.search) throw new LinkError("malformed");
  } else if (hash < 0 && /^[a-z][a-z0-9+.-]*:/i.test(input)) {
    throw new LinkError("malformed");
  }
  const fragment = hash >= 0 ? input.slice(hash + 1) : input;
  const fields = new Map();
  for (const pair of fragment.split("&")) {
    const eq = pair.indexOf("=");
    if (eq <= 0) throw new LinkError("malformed");
    const name = pair.slice(0, eq);
    const value = pair.slice(eq + 1);
    if (!FIELDS.includes(name) || fields.has(name)) throw new LinkError("malformed");
    if (!PATTERNS[name].test(value)) throw new LinkError("malformed");
    fields.set(name, value);
  }
  if (fields.size !== FIELDS.length) throw new LinkError("malformed");
  return Object.fromEntries(fields);
}

export const fromHex = (hex) => Uint8Array.from(hex.match(/../g) ?? [], (x) => parseInt(x, 16));
export const toHex = (bytes) => Array.from(bytes, (x) => x.toString(16).padStart(2, "0")).join("");
export const randomId = (random = crypto.getRandomValues.bind(crypto)) => toHex(random(new Uint8Array(16)));

// Groups of four for reading aloud or comparing with the list at home.
export function fingerprint(publicKey) {
  return toHex(publicKey).match(/.{4}/g).join(" ");
}

// The protocol's name field: 1-255 UTF-8 bytes, no control characters.
export function uploadName(name) {
  const clean = String(name ?? "").replace(/[\x00-\x1f\x7f]/g, "").trim();
  let out = "";
  let size = 0;
  for (const ch of clean) {
    const n = enc.encode(ch).length;
    if (size + n > 255) break;
    out += ch;
    size += n;
  }
  return out || "photo";
}

// The protocol's MIME field: 0-127 printable ASCII; anything odd is "unspecified".
export function uploadType(type) {
  const value = String(type ?? "").toLowerCase();
  return /^[a-z0-9.+-]{1,40}\/[a-z0-9.+-]{1,80}$/.test(value) && value.length <= 127 ? value : "";
}

// Raster types the page may hand to <img>. SVG and anything scriptable are not
// here: a Blob URL is same-origin, so it must never carry a document type.
const RASTER = new Map([
  ["jpg", "image/jpeg"],
  ["jpeg", "image/jpeg"],
  ["png", "image/png"],
  ["gif", "image/gif"],
  ["webp", "image/webp"],
  ["avif", "image/avif"],
  ["heic", "image/heic"],
  ["heif", "image/heif"],
  ["bmp", "image/bmp"],
]);
const RASTER_TYPES = new Set(RASTER.values());

export function extensionOf(key) {
  const name = String(key).split("/").pop() ?? "";
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(dot + 1).toLowerCase() : "";
}

// Returns the fixed image type to label a Blob with, or null for "a file".
export function imageType(key, mime) {
  const declared = String(mime ?? "").toLowerCase();
  if (RASTER_TYPES.has(declared)) return declared;
  if (declared && declared !== "application/octet-stream") return null;
  return RASTER.get(extensionOf(key)) ?? null;
}

// Grid tiles load originals; very large files wait for a tap instead.
export const TILE_LIMIT = 16 * 1024 * 1024;

export function kindLabel(key, mime) {
  const ext = extensionOf(key);
  if (ext && ext.length <= 5) return ext;
  const sub = String(mime ?? "").split("/")[1] ?? "";
  return /^[a-z0-9]{1,5}$/.test(sub) ? sub : "file";
}

// Newest first: keys start with the UTC date and time of the upload.
export function newestFirst(items) {
  return [...items].sort((a, b) => (a.key < b.key ? 1 : a.key > b.key ? -1 : 0));
}

// Blob URLs the page created, bounded by count and bytes. Evicting revokes the
// URL and tells the owner so a tile can drop its image. Pinned entries (the
// open preview) are never evicted.
export class BlobCache {
  constructor({ entries = 40, bytes = 96 * 1024 * 1024, create, revoke, onEvict = () => {} }) {
    Object.assign(this, { maxEntries: entries, maxBytes: bytes, create, revoke, onEvict });
    this.map = new Map();
    this.size = 0;
  }
  get(key) {
    const entry = this.map.get(key);
    if (!entry) return null;
    this.map.delete(key);
    this.map.set(key, entry);
    return entry;
  }
  put(key, blob) {
    this.remove(key);
    const entry = { key, blob, url: this.create(blob), pins: 0 };
    this.map.set(key, entry);
    this.size += blob.size;
    // The new entry is in use by its caller, so it is never the one evicted.
    this.trim(entry);
    return entry;
  }
  pin(key) {
    const entry = this.map.get(key);
    if (entry) entry.pins++;
    return entry;
  }
  unpin(key) {
    const entry = this.map.get(key);
    if (entry && entry.pins > 0) entry.pins--;
    this.trim();
  }
  remove(key) {
    const entry = this.map.get(key);
    if (!entry) return;
    this.map.delete(key);
    this.size -= entry.blob.size;
    this.revoke(entry.url);
    this.onEvict(key);
  }
  trim(keep) {
    for (const entry of [...this.map.values()]) {
      if (this.map.size <= this.maxEntries && this.size <= this.maxBytes) return;
      if (!entry.pins && entry !== keep) this.remove(entry.key);
    }
  }
  clear() {
    for (const key of [...this.map.keys()]) this.remove(key);
  }
}

// One queue per phone. Lower priority numbers run first; equal priorities run
// in order. Queued work can be cancelled or promoted; running work finishes.
export const PRIORITY = Object.freeze({ open: 0, list: 1, upload: 2, tile: 3 });

export class WorkQueue {
  constructor(run) {
    this.run = run;
    this.items = [];
    this.serial = 0;
    this.running = null;
    this.paused = false;
  }
  add(priority, work) {
    let settle;
    const done = new Promise((resolve, reject) => (settle = { resolve, reject }));
    const item = { priority, work, order: this.serial++, settle, cancelled: false };
    item.cancel = () => this.cancel(item);
    // Promoted work is what the user just asked for: front of its new class.
    item.promote = (p) => {
      if (p >= item.priority) return;
      item.priority = p;
      item.order = -this.serial++;
      this.sort();
    };
    item.done = done;
    this.items.push(item);
    this.sort();
    this.pump();
    return item;
  }
  sort() {
    this.items.sort((a, b) => a.priority - b.priority || a.order - b.order);
  }
  cancel(item) {
    const index = this.items.indexOf(item);
    if (index < 0) return false;
    this.items.splice(index, 1);
    item.cancelled = true;
    item.settle.reject(new CancelledError());
    return true;
  }
  clear() {
    for (const item of [...this.items]) this.cancel(item);
  }
  pause() {
    this.paused = true;
  }
  resume() {
    this.paused = false;
    this.pump();
  }
  get busy() {
    return Boolean(this.running);
  }
  get length() {
    return this.items.length;
  }
  async pump() {
    if (this.running || this.paused) return;
    const item = this.items.shift();
    if (!item) return;
    this.running = item;
    item.started = true;
    try {
      item.onStart?.();
      item.settle.resolve(await this.run(item.work, item));
    } catch (error) {
      item.settle.reject(error);
    } finally {
      this.running = null;
      queueMicrotask(() => this.pump());
    }
  }
}

export class CancelledError extends Error {
  constructor() {
    super("cancelled");
    this.reason = "cancelled";
  }
}

// Begun uploads whose outcome home has not confirmed: ID, name and size only.
// An entry leaves only when its outcome is settled or the owner dismisses it,
// never by age or count. At the bound, a new upload is refused before
// put-begin is sent, so every attempt home may hold can still be checked.
export const MAX_UNSETTLED = 50;
export class UploadLog {
  constructor(store, { max = MAX_UNSETTLED } = {}) {
    this.store = store;
    this.max = max;
    this.chain = Promise.resolve();
  }
  // Read-modify-write steps run one at a time so none loses another's entry.
  #update(change) {
    const next = this.chain.then(async () => {
      const list = (await this.store.get("uploads")) ?? [];
      const out = change(list);
      if (out !== list) await this.store.put("uploads", out);
      return out;
    });
    this.chain = next.catch(() => {});
    return next;
  }
  entries() {
    return this.#update((list) => list);
  }
  remember({ id, name, size }) {
    return this.#update((list) => {
      if (list.some((u) => u.id === id)) return list;
      if (list.length >= this.max) throw new LinkError("unsettled");
      return [...list, { id, name, size, at: Date.now() }];
    });
  }
  settle(id) {
    return this.#update((list) => (list.some((u) => u.id === id) ? list.filter((u) => u.id !== id) : list));
  }
}
