import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { Session, keyPair, publicBytes } from "../../web/protocol/noise.mjs";
import {
  encode,
  decode,
  envelope,
  unwrap,
  ack,
  tokenSet,
} from "../../web/protocol/wire.mjs";
import { hash, APPLICATION_WINDOW, ACK_INTERVAL } from "../limits.mjs";
const token = "cd".repeat(32),
  unhex = (s) => new Uint8Array(Buffer.from(s, "hex"));
const digest = (b) => createHash("sha256").update(b).digest("hex");
export async function transfer(io) {
  const b = await io.connect("bridge", "ab".repeat(32));
  await io.send(b, tokenSet(unhex(await hash(unhex(token))), 1, 42));
  await io.send(b, envelope("ping", 0, new Uint8Array(8)));
  assert.equal(unwrap(await io.read(b)).kind, "pong");
  const sessions = [];
  for (const mode of ["enroll", "reconnect"]) {
    const p = await io.connect("phone", token),
      opened = unwrap(await io.read(p));
    assert.equal(opened.kind, "open");
    assert.deepEqual(unwrap(await io.read(b)), opened);
    const ka = await keyPair(),
      kb = await keyPair(),
      psk = new Uint8Array(32);
    const i = await Session.create({
      mode,
      route: "example",
      psk,
      initiator: true,
      privateKey: ka.privateKey,
      publicKey: await publicBytes(ka.publicKey),
      remoteStatic: await publicBytes(kb.publicKey),
    });
    const r = await Session.create({
      mode,
      route: "example",
      psk,
      initiator: false,
      privateKey: kb.privateKey,
      publicKey: await publicBytes(kb.publicKey),
    });
    const s = {
      p,
      i,
      r,
      id: opened.session,
      up: 0n,
      down: 0n,
      upAck: 0n,
      downAck: 0n,
    };
    sessions.push(s);
    const move = async (from, to, payload, down) => {
      await io.send(from, envelope("data", s.id, payload));
      const e = unwrap(await io.read(to));
      assert.equal(e.kind, "data");
      assert.equal(e.session, s.id);
      const dir = down ? "down" : "up";
      s[dir] += BigInt(e.data.length + 6);
      await io.send(to, ack(s.id, s[dir]));
      return e.data;
    };
    await r.readHandshake(await move(p, b, await i.writeHandshake(), false));
    await i.readHandshake(await move(b, p, await r.writeHandshake(), true));
    if (mode === "enroll")
      await r.readHandshake(await move(p, b, await i.writeHandshake(), false));
  }
  const [a, c] = sessions,
    length = 10 * 1024 * 1024,
    source = Buffer.alloc(length);
  for (let n = 0; n < length; n++) source[n] = n % 251;
  const up = createHash("sha256"),
    down = createHash("sha256");
  const id = "01".repeat(16);
  let offset = 0,
    index = 0,
    maxBurst = 0;
  while (offset < length) {
    const queued = [];
    let burst = 0;
    for (let n = 0; n < 8 && offset < length; n++, index++) {
      const chunk = new Uint8Array(
        source.subarray(offset, Math.min(offset + 61440, length)),
      );
      const plain = encode({ type: "chunk", id, index, data: chunk });
      const u = envelope("data", a.id, await a.i.encrypt(plain)),
        d = envelope("data", c.id, await c.r.encrypt(plain));
      burst += Math.max(u.length, d.length);
      assert.ok(burst <= APPLICATION_WINDOW);
      await io.send(a.p, u);
      await io.send(b, d);
      queued.push(chunk);
      offset += chunk.length;
    }
    maxBurst = Math.max(maxBurst, burst);
    for (const expected of queued) {
      const u = unwrap(await io.read(b)),
        d = unwrap(await io.read(c.p));
      assert.equal(u.kind, "data");
      assert.equal(d.kind, "data");
      const x = decode(await a.r.decrypt(u.data)).data,
        y = decode(await c.i.decrypt(d.data)).data;
      assert.deepEqual(x, expected);
      assert.deepEqual(y, expected);
      up.update(x);
      down.update(y);
      a.up += BigInt(u.data.length + 6);
      c.down += BigInt(d.data.length + 6);
      if (a.up - a.upAck >= BigInt(ACK_INTERVAL)) {
        await io.send(b, ack(a.id, a.up));
        a.upAck = a.up;
      }
      if (c.down - c.downAck >= BigInt(ACK_INTERVAL)) {
        await io.send(c.p, ack(c.id, c.down));
        c.downAck = c.down;
      }
    }
    // Idle flush: even a burst shorter than the ACK interval releases its window.
    await io.send(b, ack(a.id, a.up));
    a.upAck = a.up;
    await io.send(c.p, ack(c.id, c.down));
    c.downAck = c.down;
    if (io.pause) await io.pause();
  }
  assert.equal(up.digest("hex"), digest(source));
  assert.equal(down.digest("hex"), digest(source));
  return {
    bytes: length,
    sha256: digest(source),
    maxBurst,
    sessions: sessions.map((s) => s.id),
  };
}
