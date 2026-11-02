// Device key and pairing records in IndexedDB.
//
// Safari reads a stored X25519 CryptoKey back as null (WebKit bug 312279), so
// the page stores a non-extractable AES-GCM key and the X25519 private key as
// its `wrapKey("pkcs8")` output with the IV. Loading unwraps a non-extractable
// copy. The extractable original exists only inside `create()`.
const NAME = "homepool-remote";
const STORE = "records";

function request(r) {
  return new Promise((resolve, reject) => {
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
}

export function openStore(indexedDB = globalThis.indexedDB) {
  let db = null;
  const connect = async () => {
    if (db) return db;
    const open = indexedDB.open(NAME, 1);
    open.onupgradeneeded = () => open.result.createObjectStore(STORE);
    db = await request(open);
    db.onversionchange = () => {
      db.close();
      db = null;
    };
    return db;
  };
  const run = async (mode, fn) => {
    const tx = (await connect()).transaction(STORE, mode);
    const result = await request(fn(tx.objectStore(STORE)));
    await new Promise((resolve, reject) => {
      tx.oncomplete = resolve;
      tx.onerror = tx.onabort = () => reject(tx.error);
    });
    return result;
  };
  return {
    get: (key) => run("readonly", (s) => s.get(key)),
    put: (key, value) => run("readwrite", (s) => s.put(value, key)),
    del: (key) => run("readwrite", (s) => s.delete(key)),
    // Forget: clear every record, then drop the database itself.
    async destroy() {
      try {
        await run("readwrite", (s) => s.clear());
      } finally {
        db?.close();
        db = null;
        await new Promise((resolve) => {
          const r = indexedDB.deleteDatabase(NAME);
          r.onsuccess = r.onerror = r.onblocked = () => resolve();
        });
      }
    },
  };
}

export function deviceKeys(store, subtle = crypto.subtle) {
  const wrapping = async () => {
    const existing = await store.get("wrapping");
    if (existing) return existing;
    const key = await subtle.generateKey({ name: "AES-GCM", length: 256 }, false, ["wrapKey", "unwrapKey"]);
    await store.put("wrapping", key);
    // Read it back: only a key that survives storage can unwrap later.
    const stored = await store.get("wrapping");
    if (!stored) throw Object.assign(new Error("storage"), { reason: "unsupported" });
    return stored;
  };
  const unwrap = async (wrapped, iv, key) =>
    subtle.unwrapKey("pkcs8", wrapped, key, { name: "AES-GCM", iv }, { name: "X25519" }, false, ["deriveBits"]);
  return {
    async create() {
      const key = await wrapping();
      const pair = await subtle.generateKey({ name: "X25519" }, true, ["deriveBits"]);
      const iv = crypto.getRandomValues(new Uint8Array(12));
      const wrapped = new Uint8Array(await subtle.wrapKey("pkcs8", pair.privateKey, key, { name: "AES-GCM", iv }));
      const publicKey = new Uint8Array(await subtle.exportKey("raw", pair.publicKey));
      return { privateKey: await unwrap(wrapped, iv, key), publicKey, wrapped, iv };
    },
    async restore(record) {
      const key = await store.get("wrapping");
      if (!key || !record?.wrapped) throw Object.assign(new Error("key"), { reason: "removed" });
      return unwrap(record.wrapped, record.iv, key);
    },
  };
}

// True when this browser has every primitive the page needs.
export async function supported() {
  try {
    if (!globalThis.isSecureContext || !crypto?.subtle || !globalThis.indexedDB || !globalThis.WebSocket) return false;
    await crypto.subtle.generateKey({ name: "X25519" }, false, ["deriveBits"]);
    return typeof Blob.prototype.arrayBuffer === "function";
  } catch {
    return false;
  }
}
