// Noise revision 34: only the two homepool patterns; WebCrypto throughout.
const te = new TextEncoder();
export const bytes = (s) => te.encode(s);
export const concat = (...xs) => {
  const out = new Uint8Array(xs.reduce((n, x) => n + x.length, 0));
  let n = 0;
  for (const x of xs) {
    out.set(x, n);
    n += x.length;
  }
  return out;
};
const empty = new Uint8Array();
const hash = async (x) =>
  new Uint8Array(await crypto.subtle.digest("SHA-256", x));
async function hmac(k, x) {
  return new Uint8Array(
    await crypto.subtle.sign(
      "HMAC",
      await crypto.subtle.importKey(
        "raw",
        k,
        { name: "HMAC", hash: "SHA-256" },
        false,
        ["sign"],
      ),
      x,
    ),
  );
}
async function hkdf(k, x, n = 2) {
  const t = await hmac(k, x);
  const out = [];
  let p = empty;
  for (let i = 1; i <= n; i++) {
    p = await hmac(t, concat(p, new Uint8Array([i])));
    out.push(p);
  }
  return out;
}
export async function keyPair() {
  return crypto.subtle.generateKey({ name: "X25519" }, false, ["deriveBits"]);
}
export async function publicBytes(k) {
  return new Uint8Array(await crypto.subtle.exportKey("raw", k));
}
// Only for published test vectors; production accepts CryptoKey private keys.
export async function vectorKey(raw) {
  const privateKey = await crypto.subtle.importKey(
    "pkcs8",
    concat(
      new Uint8Array([
        48, 46, 2, 1, 0, 48, 5, 6, 3, 43, 101, 110, 4, 34, 4, 32,
      ]),
      raw,
    ),
    { name: "X25519" },
    false,
    ["deriveBits"],
  );
  return privateKey;
}
async function dh(k, p) {
  const pub = await crypto.subtle.importKey("raw", p, "X25519", false, []);
  return new Uint8Array(
    await crypto.subtle.deriveBits({ name: "X25519", public: pub }, k, 256),
  );
}
class Cipher {
  constructor(k) {
    this.k = k;
    this.n = 0n;
  }
  async run(data, ad, decrypt = false) {
    if (!this.k) return data;
    if (this.n >= 0xffffffffffffffffn) throw Error("nonce exhausted");
    const iv = new Uint8Array(12);
    new DataView(iv.buffer).setBigUint64(4, this.n, false);
    const key = await crypto.subtle.importKey("raw", this.k, "AES-GCM", false, [
      decrypt ? "decrypt" : "encrypt",
    ]);
    const out = new Uint8Array(
      await crypto.subtle[decrypt ? "decrypt" : "encrypt"](
        { name: "AES-GCM", iv, additionalData: ad, tagLength: 128 },
        key,
        data,
      ),
    );
    this.n++;
    return out;
  }
}
// Internal primitive exposed for community-vector tests, which contain payloads.
// Applications must use Session below, which forbids handshake payloads.
export class NoiseCore {
  static async create({
    mode,
    initiator,
    privateKey,
    publicKey,
    remoteStatic,
    psk,
    prologue = empty,
    ephemeral,
  }) {
    if (!["enroll", "reconnect"].includes(mode)) throw Error("mode");
    if (mode === "enroll" && psk?.length !== 32) throw Error("PSK");
    const s = new NoiseCore();
    Object.assign(s, {
      mode,
      initiator,
      privateKey,
      publicKey,
      remoteStatic,
      psk,
      ephemeral,
      step: 0,
    });
    s.pattern =
      mode === "enroll"
        ? [
            ["e", "es"],
            ["e", "ee"],
            ["s", "se", "psk"],
          ]
        : [
            ["e", "es", "s", "ss"],
            ["e", "ee", "se"],
          ];
    const name = bytes(
      `Noise_${mode === "enroll" ? "XKpsk3" : "IK"}_25519_AESGCM_SHA256`,
    );
    s.h =
      name.length <= 32
        ? concat(name, new Uint8Array(32 - name.length))
        : await hash(name);
    s.ck = s.h;
    s.c = new Cipher();
    await s.mixHash(prologue);
    await s.mixHash(initiator ? remoteStatic : publicKey);
    return s;
  }
  async mixHash(x) {
    this.h = await hash(concat(this.h, x));
  }
  async mixKey(x) {
    const [ck, k] = await hkdf(this.ck, x);
    this.ck = ck;
    this.c = new Cipher(k);
  }
  async crypt(x, read) {
    const out = await this.c.run(x, this.h, read);
    await this.mixHash(read ? x : out);
    return out;
  }
  async message(read, input = empty) {
    if (
      this.step >= this.pattern.length ||
      read === ((this.step % 2 === 0) === this.initiator)
    )
      throw Error("handshake order");
    let offset = 0;
    const out = [];
    const take = (n) => {
      if (offset + n > input.length) throw Error("short handshake");
      const v = input.slice(offset, offset + n);
      offset += n;
      return v;
    };
    for (const token of this.pattern[this.step]) {
      if (token === "e") {
        if (read) {
          this.re = take(32);
          await this.mixHash(this.re);
          if (this.mode === "enroll") await this.mixKey(this.re);
        } else {
          this.ephemeral ??= await keyPair();
          this.e = this.ephemeral.privateKey;
          const p = await publicBytes(this.ephemeral.publicKey);
          out.push(p);
          await this.mixHash(p);
          if (this.mode === "enroll") await this.mixKey(p);
        }
      } else if (token === "s") {
        if (read)
          this.remoteStatic = await this.crypt(take(this.c.k ? 48 : 32), true);
        else out.push(await this.crypt(this.publicKey, false));
      } else if (token === "psk") {
        const [ck, h, k] = await hkdf(this.ck, this.psk, 3);
        this.ck = ck;
        await this.mixHash(h);
        this.c = new Cipher(k);
      } else {
        const local =
          token === "ee" ||
          (token === "es" && this.initiator) ||
          (token === "se" && !this.initiator)
            ? this.e
            : this.privateKey;
        const remote =
          token === "ee" ||
          (token === "es" && !this.initiator) ||
          (token === "se" && this.initiator)
            ? this.re
            : this.remoteStatic;
        await this.mixKey(await dh(local, remote));
      }
    }
    const payload = read
      ? await this.crypt(input.slice(offset), true)
      : await this.crypt(input, false);
    if (!read) out.push(payload);
    this.step++;
    if (this.step === this.pattern.length) {
      const [a, b] = await hkdf(this.ck, empty);
      this.send = new Cipher(this.initiator ? a : b);
      this.recv = new Cipher(this.initiator ? b : a);
    }
    return read ? payload : concat(...out);
  }
}
export function prologue(mode, route) {
  if (
    !["enroll", "reconnect"].includes(mode) ||
    !/^[A-Za-z0-9_-]{1,64}$/.test(route)
  )
    throw Error("prologue");
  return bytes(`homepool/1/${mode}/${route}`);
}
export class Session {
  #core;
  #queue = Promise.resolve();
  #failed = false;
  static async create(options) {
    const s = new Session();
    s.#core = await NoiseCore.create({
      ...options,
      prologue: prologue(options.mode, options.route),
    });
    return s;
  }
  #run(fn) {
    const task = this.#queue.then(async () => {
      if (this.#failed) throw Error("session closed");
      try {
        return await fn();
      } catch (e) {
        this.#failed = true;
        throw e;
      }
    });
    this.#queue = task.catch(() => {});
    return task;
  }
  writeHandshake() {
    return this.#run(() => this.#core.message(false));
  }
  readHandshake(data) {
    return this.#run(async () => {
      if (data.length > 65535) throw Error("size");
      if ((await this.#core.message(true, data)).length)
        throw Error("handshake payload");
    });
  }
  get remoteStatic() {
    return this.#core.remoteStatic?.slice();
  }
  encrypt(data) {
    return this.#run(async () => {
      if (!this.#core.send || data.length > 65519)
        throw Error("transport size/state");
      return this.#core.send.run(data, empty);
    });
  }
  decrypt(data) {
    return this.#run(async () => {
      if (!this.#core.recv || data.length < 16 || data.length > 65535)
        throw Error("transport size/state");
      return this.#core.recv.run(data, empty, true);
    });
  }
}
