// The page's connection code against a scripted relay and bridge in memory:
// real Noise on both ends, the real wire codec, and the relay's framing,
// close codes and receive window checked on every message.
import assert from "node:assert/strict";
import { createHash, randomBytes } from "node:crypto";
import { test } from "node:test";
import { Session, keyPair, publicBytes } from "../../protocol/noise.mjs";
import { decode, encode, envelope, readAdmission, unwrap } from "../../protocol/wire.mjs";
import { CHUNK, CLOSE, Remote, TIMES, get, list, put, status } from "../client.mjs";
import { deviceKeys } from "../keys.mjs";
import { toHex } from "../model.mjs";

TIMES.retry = 20;
const hex = (b) => Buffer.from(b).toString("hex");
const sha = (b) => createHash("sha256").update(b).digest("hex");
const key = (n) => `2026-11-03/120000-${"a".repeat(25)}a-photo${n}.jpg`;

function memoryStore() {
  const map = new Map();
  return {
    map,
    get: async (k) => map.get(k),
    put: async (k, v) => void map.set(k, v),
    del: async (k) => void map.delete(k),
    destroy: async () => map.clear(),
  };
}

// One relay route with one bridge. `tokens` maps a token to
// { mode, psk?, device? }; `refuse` forces an admission close code.
async function home({ objects = new Map() } = {}) {
  const pair = await keyPair();
  const h = {
    bridgeKey: hex(await publicBytes(pair.publicKey)),
    tokens: new Map(),
    refuse: null,
    unreachable: false,
    objects,
    statuses: new Map(),
    uploads: [],
    devices: new Map(),
    dropUploadAfter: Infinity,
    withholdEnrolled: false,
    sockets: [],
    maxUnacked: 0,
    maxCredit: 0,
  };
  let nextSession = 1;
  class FakeSocket {
    constructor(url) {
      this.url = url;
      this.listeners = {};
      this.bufferedAmount = 0;
      this.closed = false;
      this.chain = Promise.resolve();
      h.sockets.push(this);
      setTimeout(() => {
        if (h.unreachable) {
          this.emit("error", {});
          return this.close(1006);
        }
        this.emit("open", {});
      }, 1);
    }
    addEventListener(type, fn) {
      (this.listeners[type] ??= []).push(fn);
    }
    emit(type, event) {
      for (const fn of this.listeners[type] ?? []) fn(event);
    }
    close(code = 1000) {
      if (this.closed) return;
      this.closed = true;
      setTimeout(() => this.emit("close", { code }), 1);
    }
    down(kind, data) {
      if (this.closed) return;
      const bytes = envelope(kind, this.id ?? 0, data);
      if (kind === "data") {
        this.sent += bytes.length;
        h.maxUnacked = Math.max(h.maxUnacked, this.sent - this.acked);
        // The relay's receive window: never more than 1 MiB unacknowledged.
        assert.ok(this.sent - this.acked <= 1024 * 1024, "relay window exceeded");
      }
      setTimeout(() => !this.closed && this.emit("message", { data: bytes.slice().buffer }), 0);
    }
    send(bytes) {
      if (this.closed) return; // a closing socket drops sends, as browsers do
      this.frames = (this.frames ?? 0) + 1;
      const copy = new Uint8Array(bytes);
      this.chain = this.chain.then(() => this.receive(copy)).catch((e) => {
        this.error = e;
        if (process.env.DEBUG_FAKE) console.error("fake:", e);
        this.close(1008);
      });
    }
    async reply(m) {
      this.down("data", await this.noise.encrypt(encode(m)));
    }
    async receive(bytes) {
      if (this.closed) return;
      if (!this.admitted) {
        const { role, token } = readAdmission(bytes);
        assert.equal(role, "phone");
        if (h.refuse) return this.close(h.refuse);
        const t = h.tokens.get(token);
        if (!t) return this.close(CLOSE.notAccepted);
        this.admitted = t;
        this.id = nextSession++;
        this.sent = 0;
        this.acked = 0;
        this.down("open", new Uint8Array(4));
        this.noise = await Session.create({
          mode: t.mode,
          initiator: false,
          route: "example",
          privateKey: pair.privateKey,
          publicKey: await publicBytes(pair.publicKey),
          psk: t.psk,
        });
        this.step = 0;
        return;
      }
      const e = unwrap(bytes);
      assert.equal(e.session, this.id);
      if (e.kind === "ack") {
        const n = Number(new DataView(e.data.buffer).getBigUint64(0));
        assert.ok(n >= this.acked && n <= this.sent, "ack in range");
        this.acked = n;
        return;
      }
      assert.equal(e.kind, "data");
      const t = this.admitted;
      if (this.step === 0) {
        await this.noise.readHandshake(e.data);
        this.step = 1;
        // Like the bridge: check the device key after message 1.
        if (t.mode === "reconnect" && hex(this.noise.remoteStatic) !== h.devices.get(t.device)?.key)
          return this.close(1000);
        this.down("data", await this.noise.writeHandshake());
        if (t.mode === "reconnect") {
          this.step = 2;
          if (t.invitation) await this.reply({ type: "enrolled", id: t.device, token: h.devices.get(t.device).token });
        }
        return;
      }
      if (this.step === 1) {
        await this.noise.readHandshake(e.data);
        this.step = 2;
        const id = randomBytes(16).toString("hex");
        const token = randomBytes(32).toString("hex");
        h.devices.set(id, { key: hex(this.noise.remoteStatic), token });
        h.tokens.set(token, { mode: "reconnect", device: id });
        // The used invitation now only admits that device's recovery.
        Object.assign(t, { mode: "reconnect", device: id, invitation: true, psk: undefined });
        if (h.withholdEnrolled) return this.close(1000);
        return this.reply({ type: "enrolled", id, token });
      }
      await this.app(decode(await this.noise.decrypt(e.data)));
    }
    async app(m) {
      if (m.type === "list") {
        const items = [...h.objects].map(([k, v]) => ({ key: k, length: v.length, type: "image/jpeg" }));
        return this.reply({ type: "list-page", id: m.id, cursor: "", items });
      }
      if (m.type === "ping") return this.reply({ type: "pong", id: m.id });
      if (m.type === "put-status") {
        const known = h.statuses.get(m.id) ?? "unknown";
        return this.reply(typeof known === "string" ? { type: known, id: m.id } : { ...known, id: m.id });
      }
      if (m.type === "get") {
        const data = h.objects.get(m.key);
        if (!data) return this.reply({ type: "error", id: m.id, code: "refused" });
        this.download = { id: m.id, data, at: 0, index: 0, credits: 0 };
        return this.reply({ type: "get-head", id: m.id, length: data.length, mime: "image/jpeg" });
      }
      if (m.type === "credit") {
        const d = this.download;
        d.credits += m.chunks;
        h.maxCredit = Math.max(h.maxCredit, d.credits);
        assert.ok(d.credits <= 8, "credit window");
        while (d.credits > 0 && d.at < d.data.length) {
          const chunk = d.data.subarray(d.at, d.at + CHUNK);
          await this.reply({ type: "chunk", id: d.id, index: d.index++, data: chunk });
          d.at += chunk.length;
          d.credits--;
        }
        if (d.at === d.data.length && !d.ended) {
          d.ended = true;
          await this.reply({ type: "get-end", id: d.id, length: d.data.length, count: d.index });
        }
        return;
      }
      if (m.type === "put-begin") {
        assert.ok(!h.statuses.has(m.id), "request ID reused");
        h.statuses.set(m.id, "pending");
        this.upload = { ...m, parts: [], credits: 8 };
        return this.reply({ type: "credit", id: m.id, chunks: 8 });
      }
      if (m.type === "chunk") {
        const u = this.upload;
        assert.ok(u.credits > 0, "chunk without credit");
        u.credits--;
        u.parts.push(m.data);
        if (u.parts.length >= h.dropUploadAfter) return this.close(1008);
        if (u.credits === 0) {
          u.credits = 8;
          await this.reply({ type: "credit", id: m.id, chunks: 8 });
        }
        return;
      }
      if (m.type === "put-end") {
        const data = Buffer.concat(this.upload.parts);
        assert.equal(data.length, m.length);
        h.uploads.push({ name: this.upload.name, mime: this.upload.mime, sha: sha(data), count: m.count });
        const k = key(h.uploads.length);
        h.statuses.set(m.id, {
          type: "committed",
          key: k,
          name: this.upload.name,
          mime: this.upload.mime,
          length: data.length,
          hash: sha(data),
        });
        h.objects.set(k, data);
        return this.reply({ type: "put-done", id: m.id, key: k });
      }
      throw Error(`unexpected ${m.type}`);
    }
  }
  h.WebSocketImpl = FakeSocket;
  h.invite = () => {
    const invite = {
      route: "example",
      token: randomBytes(32).toString("hex"),
      bridge: h.bridgeKey,
      invitation: randomBytes(16).toString("hex"),
      psk: randomBytes(32).toString("hex"),
    };
    h.tokens.set(invite.token, { mode: "enroll", psk: Buffer.from(invite.psk, "hex") });
    return invite;
  };
  return h;
}

function phone(h, store = memoryStore()) {
  const statuses = [];
  const removed = [];
  const remote = new Remote({
    origin: ["https", "//192.0.2.1"].join(":"),
    store,
    keys: deviceKeys(store),
    WebSocketImpl: h.WebSocketImpl,
    onStatus: (s, r) => statuses.push(r ? `${s}:${r}` : s),
    onRemoved: (r) => removed.push(r),
  });
  return { remote, store, statuses, removed };
}

test("pairs, stores only the wrapped key and reconnects with the device token", async () => {
  const h = await home();
  const p = phone(h);
  const steps = [];
  const device = await p.remote.pair(h.invite(), { onStep: (s) => steps.push(s) });
  assert.deepEqual(steps, ["connecting", "checking", "saving"]);
  assert.equal(h.devices.get(device.id).key, toHex(device.publicKey));
  assert.deepEqual(Object.keys(device).sort(), ["bridge", "iv", "id", "paired", "publicKey", "route", "token", "wrapped"].sort());
  assert.equal(p.store.map.has("pending"), false);
  const wrapping = p.store.map.get("wrapping");
  assert.equal(wrapping.extractable, false);
  const restored = await deviceKeys(p.store).restore(device);
  await assert.rejects(crypto.subtle.exportKey("pkcs8", restored));
  // A second page load: new Remote, same storage, device token admission.
  p.remote.disconnect();
  const again = phone(h, p.store);
  assert.equal((await again.remote.load()).id, device.id);
  const link = await again.remote.ensure();
  assert.equal(h.sockets.at(-1).admitted.device, device.id);
  assert.deepEqual((await list(link)).items, []);
  again.remote.disconnect();
});

test("downloads and uploads respect credits and the relay window", async () => {
  const data = randomBytes(1024 * 1024 + 7);
  const h = await home({ objects: new Map([[key(0), data]]) });
  const p = phone(h);
  await p.remote.pair(h.invite());
  const link = await p.remote.ensure();
  const fractions = [];
  const got = await get(link, key(0), { onProgress: (f) => fractions.push(f) });
  assert.equal(sha(Buffer.from(await got.blob.arrayBuffer())), sha(data));
  assert.equal(fractions.at(-1), 1);
  assert.ok(h.maxCredit <= 8 && h.maxCredit > 0);
  // At most one credit window (8 chunks, 492,032 envelope bytes) is ever
  // unacknowledged, and the burst end is flushed.
  assert.ok(h.maxUnacked <= 492032, `unacked ${h.maxUnacked}`);
  await new Promise((r) => setTimeout(r, 20));
  const socket = h.sockets.at(-1);
  assert.equal(socket.acked, socket.sent);

  const file = new File([randomBytes(5 * CHUNK + 123)], "IMG\u0000_1.jpg", { type: "image/jpeg" });
  const slices = [];
  const slice = file.slice.bind(file);
  file.slice = (a, b) => {
    slices.push(b - a);
    return slice(a, b);
  };
  const up = { id: randomBytes(16).toString("hex"), file };
  const order = [];
  const { key: stored } = await put(link, up, {
    onBegin: () => order.push("begin"),
    onEnd: () => order.push("end"),
  });
  assert.deepEqual(order, ["begin", "end"]);
  assert.equal(up.accepted, true);
  assert.ok(slices.every((n) => n <= CHUNK), "one chunk read at a time");
  assert.equal(slices.length, 6);
  assert.deepEqual(h.uploads.at(-1), {
    name: "IMG_1.jpg",
    mime: "image/jpeg",
    sha: sha(Buffer.from(await file.arrayBuffer())),
    count: 6,
  });
  assert.equal(stored, key(1));
  assert.equal((await status(link, up.id)).type, "committed");
  p.remote.disconnect();
});

test("an interrupted upload is reported, then resolved by put-status", async () => {
  const h = await home();
  const p = phone(h);
  await p.remote.pair(h.invite());
  h.dropUploadAfter = 3;
  const up = { id: randomBytes(16).toString("hex"), file: new File([randomBytes(20 * CHUNK)], "big.jpg") };
  let begun = false;
  const error = await p.remote.run(1, (link) => put(link, up, { onBegin: () => (begun = true) }), { retry: false }).done.then(
    () => null,
    (e) => e,
  );
  assert.equal(error?.reason, "interrupted", String(error?.stack));
  assert.ok(begun);
  const answer = await p.remote.run(1, (link) => status(link, up.id)).done;
  assert.equal(answer.type, "pending");
  h.statuses.set(up.id, "failed");
  assert.equal((await p.remote.run(1, (link) => status(link, up.id)).done).type, "failed");
  assert.equal(
    (await p.remote.run(1, (link) => status(link, randomBytes(16).toString("hex"))).done).type,
    "unknown",
  );
  p.remote.disconnect();
});

test("admission close codes become plain reasons; removal needs two refusals", async () => {
  const h = await home();
  const p = phone(h);
  const device = await p.remote.pair(h.invite());
  p.remote.disconnect();
  h.refuse = CLOSE.homeOffline;
  await assert.rejects(p.remote.ensure(), (e) => e.reason === "home-offline");
  assert.equal(p.remote.queue.paused, true, "queued work stops dialling");
  h.refuse = CLOSE.busy;
  await assert.rejects(p.remote.ensure(), (e) => e.reason === "busy");
  h.refuse = null;
  h.unreachable = true;
  await assert.rejects(p.remote.ensure(), (e) => e.reason === "unreachable");
  h.unreachable = false;
  // A token missing once (home re-registering) is retried, not reported.
  h.tokens.delete(device.token);
  setTimeout(() => h.tokens.set(device.token, { mode: "reconnect", device: device.id }), 5);
  await p.remote.ensure();
  p.remote.disconnect();
  h.tokens.delete(device.token);
  await assert.rejects(p.remote.ensure(), (e) => e.reason === "removed");
  assert.deepEqual(p.removed, ["removed"]);
  // Known token, unknown key at home: not recognized.
  h.tokens.set(device.token, { mode: "reconnect", device: device.id });
  h.devices.get(device.id).key = "00".repeat(32);
  await assert.rejects(p.remote.ensure(), (e) => e.reason === "unrecognized");
});

test("refused pairing codes keep the existing pairing", async () => {
  const h = await home();
  const p = phone(h);
  const device = await p.remote.pair(h.invite());
  p.remote.disconnect();
  const wrong = { ...h.invite(), bridge: "11".repeat(32) };
  await assert.rejects(p.remote.pair(wrong), (e) => e.reason === "code-refused");
  const used = h.invite();
  await phone(h).remote.pair(used);
  await assert.rejects(p.remote.pair(used), (e) => e.reason === "code-refused");
  const expired = { ...h.invite(), token: "22".repeat(32) };
  await assert.rejects(p.remote.pair(expired), (e) => e.reason === "code-refused");
  h.refuse = CLOSE.homeOffline;
  await assert.rejects(p.remote.pair(h.invite()), (e) => e.reason === "home-offline");
  h.refuse = null;
  assert.equal((await p.store.get("device")).id, device.id);
  assert.equal(p.store.map.has("pending"), false);
  // The wrong-key attempt did not use up its invitation.
  const retry = h.invite();
  await assert.rejects(p.remote.pair({ ...retry, bridge: "11".repeat(32) }));
  const other = phone(h);
  await other.remote.pair(retry);
  other.remote.disconnect();
});

test("a lost enrollment acknowledgement is recovered on the invitation token", async () => {
  const h = await home();
  const p = phone(h);
  h.withholdEnrolled = true;
  const invite = h.invite();
  const pending = p.remote.pair(invite);
  // The first session closes before `enrolled`; recovery runs IK on the
  // invitation token with the same key and receives it.
  setTimeout(() => (h.withholdEnrolled = false), 5);
  const device = await pending;
  assert.equal(h.devices.get(device.id).key, toHex(device.publicKey));
  assert.equal(p.store.map.has("pending"), false);
  assert.equal(h.sockets.at(-1).admitted.mode, "reconnect");
  p.remote.disconnect();
});

test("forget closes the session and deletes every record", async () => {
  const h = await home();
  const p = phone(h);
  await p.remote.pair(h.invite());
  const socket = h.sockets.at(-1);
  const queued = p.remote.run(3, () => new Promise((r) => setTimeout(r, 50)));
  const waiting = p.remote.run(3, async () => "never");
  await p.remote.forget();
  assert.equal(p.store.map.size, 0);
  assert.equal(p.remote.device, null);
  assert.ok(socket.closed);
  await assert.rejects(waiting.done, (e) => e.reason === "cancelled");
  void queued;
  await assert.rejects(p.remote.ensure(), (e) => e.reason === "unpaired");
});

test("returning to the page reconnects once, without idle pings", async () => {
  const h = await home();
  const p = phone(h);
  await p.remote.pair(h.invite());
  const first = h.sockets.length;
  // Hidden while idle: the session closes and frees its slot.
  p.remote.hidden();
  assert.equal(p.remote.link, null);
  await p.remote.visible({ reconnect: true });
  assert.equal(h.sockets.length, first + 1);
  assert.ok(p.remote.link && !p.remote.link.closed);
  assert.equal(p.statuses.at(-1), "connected");
  // A live link after a blink stays; a long absence replaces it, once.
  p.remote.hiddenAt = Date.now();
  await p.remote.visible({ reconnect: true });
  assert.equal(h.sockets.length, first + 1);
  p.remote.hiddenAt = Date.now() - 2000;
  await p.remote.visible({ reconnect: true });
  assert.equal(h.sockets.length, first + 2);
  // Nothing is sent on an idle session: no pings, no other frames.
  const socket = h.sockets.at(-1);
  await new Promise((r) => setTimeout(r, 20));
  const frames = socket.frames;
  await new Promise((r) => setTimeout(r, 200));
  assert.equal(socket.frames, frames);
  // Away from the library (reconnect false), returning does not dial.
  p.remote.hidden();
  await p.remote.visible();
  assert.equal(h.sockets.length, first + 2);
  // A failed reconnect is reported once and does not dial again on its own.
  h.refuse = CLOSE.homeOffline;
  await p.remote.visible({ reconnect: true });
  const failed = h.sockets.length;
  assert.equal(p.statuses.at(-1), "offline:home-offline");
  assert.equal(p.remote.queue.paused, true);
  await new Promise((r) => setTimeout(r, 50));
  assert.equal(h.sockets.length, failed);
  // The next return resumes work left paused by the failure.
  const waiting = p.remote.run(3, async () => "later");
  p.remote.hidden();
  h.refuse = null;
  await p.remote.visible({ reconnect: true });
  assert.equal(await waiting.done, "later");
  p.remote.disconnect();
});
