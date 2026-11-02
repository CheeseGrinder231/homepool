import { test } from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import {
  NoiseCore,
  vectorKey,
  publicBytes,
  Session,
  keyPair,
  bytes,
} from "../noise.mjs";
export const unhex = (s) => new Uint8Array(Buffer.from(s, "hex"));
export const hex = (b) => Buffer.from(b).toString("hex");
// Derive the public key with the standard base point; never export private bytes.
export async function vectorPair(s) {
  const privateKey = await vectorKey(unhex(s));
  const base = new Uint8Array(32);
  base[0] = 9;
  const p = await crypto.subtle.importKey("raw", base, "X25519", false, []);
  const raw = await crypto.subtle.deriveBits(
    { name: "X25519", public: p },
    privateKey,
    256,
  );
  return {
    privateKey,
    publicKey: await crypto.subtle.importKey("raw", raw, "X25519", true, []),
  };
}
const vectors = JSON.parse(
  await readFile(new URL("../../../protocol/cacophony.json", import.meta.url)),
);
for (const v of vectors.filter((v) => !v.protocol_name.includes("Noise_XK_")))
  test(`published ${v.protocol_name}`, async () => {
    const mode = v.protocol_name.includes("psk") ? "enroll" : "reconnect";
    const peers = [];
    for (const side of ["init", "resp"]) {
      const k = await vectorPair(v[`${side}_static`]);
      peers.push(
        await NoiseCore.create({
          mode,
          initiator: side === "init",
          privateKey: k.privateKey,
          publicKey: await publicBytes(k.publicKey),
          remoteStatic:
            side === "init" ? unhex(v.init_remote_static) : undefined,
          psk: v.init_psks?.[0] ? unhex(v.init_psks[0]) : undefined,
          prologue: unhex(v[`${side}_prologue`]),
          ephemeral: await vectorPair(v[`${side}_ephemeral`]),
        }),
      );
    }
    for (let n = 0; n < v.messages.length; n++) {
      const sender = peers[n % 2],
        receiver = peers[1 - (n % 2)],
        m = v.messages[n];
      const payload = unhex(m.payload);
      if (n < (mode === "enroll" ? 3 : 2)) {
        assert.equal(hex(await sender.message(false, payload)), m.ciphertext);
        assert.equal(
          hex(await receiver.message(true, unhex(m.ciphertext))),
          m.payload,
        );
      } else {
        assert.equal(
          hex(await sender.send.run(payload, new Uint8Array())),
          m.ciphertext,
        );
        assert.equal(
          hex(
            await receiver.recv.run(
              unhex(m.ciphertext),
              new Uint8Array(),
              true,
            ),
          ),
          m.payload,
        );
      }
    }
    assert.equal(hex(peers[0].h), v.handshake_hash);
    assert.equal(hex(peers[1].h), v.handshake_hash);
  });
export async function pair(mode = "enroll", extra = {}) {
  const a = await keyPair(),
    b = await keyPair();
  const psk = crypto.getRandomValues(new Uint8Array(32));
  const common = { mode, route: "test", psk };
  const i = await Session.create({
    ...common,
    initiator: true,
    privateKey: a.privateKey,
    publicKey: await publicBytes(a.publicKey),
    remoteStatic: await publicBytes(b.publicKey),
    ...extra,
  });
  const r = await Session.create({
    ...common,
    initiator: false,
    privateKey: b.privateKey,
    publicKey: await publicBytes(b.publicKey),
  });
  return [i, r];
}
export async function handshake(i, r, mode = "enroll") {
  await r.readHandshake(await i.writeHandshake());
  await i.readHandshake(await r.writeHandshake());
  if (mode === "enroll") await r.readHandshake(await i.writeHandshake());
}
test("wrong bridge fails message 1", async () => {
  const [i, r] = await pair("enroll", {
    remoteStatic: await publicBytes((await keyPair()).publicKey),
  });
  await assert.rejects(r.readHandshake(await i.writeHandshake()));
});
test("wrong PSK fails message 3", async () => {
  const [i, r] = await pair("enroll", { psk: new Uint8Array(32) });
  await r.readHandshake(await i.writeHandshake());
  await i.readHandshake(await r.writeHandshake());
  await assert.rejects(r.readHandshake(await i.writeHandshake()));
});
for (const change of ["drop", "reorder", "alter"])
  test(`transport ${change} poisons session`, async () => {
    const [i, r] = await pair();
    await handshake(i, r);
    const a = await i.encrypt(bytes("a")),
      b = await i.encrypt(bytes("b"));
    const bad = change === "alter" ? a.slice() : b;
    if (change === "alter") bad[0] ^= 1;
    await assert.rejects(r.decrypt(bad));
    await assert.rejects(r.decrypt(a));
  });
test("boundary and serialization", async () => {
  const [i, r] = await pair();
  await handshake(i, r);
  const records = await Promise.all(
    Array.from({ length: 8 }, () => i.encrypt(new Uint8Array(65519))),
  );
  for (const x of records) assert.equal((await r.decrypt(x)).length, 65519);
  await assert.rejects(i.encrypt(new Uint8Array(65520)));
  await assert.rejects(i.encrypt(bytes("closed")));
});

test("session rejects authenticated handshake payloads and route mismatch", async () => {
  const a = await keyPair(),
    b = await keyPair();
  const options = {
    mode: "reconnect",
    initiator: true,
    privateKey: a.privateKey,
    publicKey: await publicBytes(a.publicKey),
    remoteStatic: await publicBytes(b.publicKey),
  };
  const { prologue } = await import("../noise.mjs");
  const core = await NoiseCore.create({
    ...options,
    prologue: prologue("reconnect", "test"),
  });
  const receiver = await Session.create({
    mode: "reconnect",
    initiator: false,
    route: "test",
    privateKey: b.privateKey,
    publicKey: await publicBytes(b.publicKey),
  });
  await assert.rejects(
    receiver.readHandshake(await core.message(false, bytes("operation"))),
  );
  const [i, r] = await pair("reconnect", { route: "different" });
  await assert.rejects(r.readHandshake(await i.writeHandshake()));
});

test("transfer rejects truncation and extra chunks", async () => {
  const { Transfer } = await import("../wire.mjs");
  const id = "01".repeat(16);
  const t = new Transfer(id, 3);
  assert.throws(() => t.accept({ type: "get-end", id, length: 3, count: 0 }));
  assert.throws(() =>
    t.accept({ type: "chunk", id, index: 0, data: new Uint8Array([1, 2, 3]) }),
  );
  const empty = new Transfer(id, 0);
  assert.equal(
    empty.accept({ type: "get-end", id, length: 0, count: 0 }),
    true,
  );
  assert.throws(() =>
    empty.accept({ type: "chunk", id, index: 0, data: new Uint8Array([1]) }),
  );
});
