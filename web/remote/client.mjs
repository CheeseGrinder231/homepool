// Remote mode: one relay WebSocket and one Noise session per phone, opened on
// demand. No DOM here; the WebSocket class, storage and key handling are passed
// in so the same code runs in Node tests against the real relay and bridge.
import { Session } from "../protocol/noise.mjs";
import { Transfer, ack, admission, decode, encode, envelope, unwrap } from "../protocol/wire.mjs";
import { CancelledError, PRIORITY, WorkQueue, fromHex, randomId, uploadName, uploadType } from "./model.mjs";

// Relay close codes for a refused phone admission (relay/route.mjs).
export const CLOSE = Object.freeze({ notAccepted: 4001, homeOffline: 4002, busy: 4003 });
export const CHUNK = 61440;
export const MAX_FILE = 50 * 1024 * 1024;
// Receiver credit window: at most eight chunks (about 492 KB with framing),
// under the 512 KiB end-to-end window. Downloads top up four at a time.
const WINDOW = 8;
const TOP_UP = 4;
const ACK_EVERY = 64 * 1024;
const SEND_BUFFER = 1024 * 1024;
export const TIMES = {
  open: 10000,
  handshake: 10000,
  response: 75000, // the bridge allows its storage worker 60 s
  progress: 30000, // file bytes or credits; pings do not count
  ping: 15000,
  pong: 10000,
  check: 5000, // ping after the page was hidden during a transfer
  retry: 8000, // second look before calling a token removed
  idle: 180000,
};

// `reason` is what the page explains; it never shows raw protocol errors.
export class RemoteError extends Error {
  constructor(reason, cause) {
    super(reason);
    this.reason = reason;
    if (cause) this.cause = cause;
  }
}
const fail = (reason, cause) => {
  throw new RemoteError(reason, cause);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function admissionFailure(code) {
  if (code === CLOSE.notAccepted) return "not-accepted";
  if (code === CLOSE.homeOffline) return "home-offline";
  if (code === CLOSE.busy) return "busy";
  return "unavailable";
}

// One admitted relay session with one Noise session. Every failure closes it.
export class Link {
  #ws;
  #session;
  #id = 0;
  #received = 0n;
  #acked = 0n;
  #phase = "socket"; // socket -> admission -> handshake -> transport
  #handshake = null; // the bridge's handshake reply
  #pending = [];
  #chain = Promise.resolve();
  #wake = new Set();
  #exchanges = new Map();
  #unrouted = [];
  #closers = new Set();
  closed = false;
  reason = null;

  static async open(options) {
    const link = new Link();
    try {
      await link.#open(options);
      return link;
    } catch (error) {
      const reason = error instanceof RemoteError ? error.reason : "refused";
      link.close(reason);
      throw error instanceof RemoteError ? error : new RemoteError(reason, error);
    }
  }

  // Resolves when `test()` returns a value, rejects when the link closes or
  // the deadline passes.
  #until(test, ms, reason) {
    return new Promise((resolve, reject) => {
      const check = () => {
        if (this.closed) return finish(reject, new RemoteError(this.reason));
        const value = test();
        if (value !== undefined && value !== null) finish(resolve, value);
      };
      const timer = setTimeout(() => finish(reject, new RemoteError(reason)), ms);
      const finish = (fn, value) => {
        clearTimeout(timer);
        this.#wake.delete(check);
        fn(value);
      };
      this.#wake.add(check);
      check();
    });
  }

  #notify() {
    for (const check of [...this.#wake]) check();
  }

  async #open({ url, WebSocketImpl, token, mode, route, privateKey, publicKey, bridge, psk, onAdmitted, onFinal }) {
    let ws;
    try {
      ws = new WebSocketImpl(url);
    } catch (error) {
      fail("unreachable", error);
    }
    this.#ws = ws;
    ws.binaryType = "arraybuffer";
    ws.addEventListener("open", () => {
      this.#phase = "admission";
      this.#notify();
    });
    ws.addEventListener("message", (event) => this.#message(event));
    ws.addEventListener("close", (event) => {
      const phase = this.#phase;
      this.close(
        phase === "socket"
          ? "unreachable"
          : phase === "admission"
            ? admissionFailure(event.code)
            : phase === "handshake"
              ? "refused"
              : "interrupted",
      );
    });
    ws.addEventListener("error", () => {
      if (this.#phase === "socket") this.close("unreachable");
    });
    await this.#until(() => (this.#phase !== "socket" ? true : null), TIMES.open, "unreachable");
    ws.send(admission("phone", token));
    await this.#until(() => (this.#phase === "handshake" ? true : null), TIMES.open, "unavailable");
    onAdmitted?.();
    this.#session = await Session.create({
      mode,
      initiator: true,
      route,
      privateKey,
      publicKey,
      remoteStatic: fromHex(bridge),
      psk: psk ? fromHex(psk) : undefined,
    });
    const handshake = async () => {
      this.#send(await this.#session.writeHandshake());
      const reply = await this.#until(() => this.#handshake, TIMES.handshake, "refused");
      await this.#session.readHandshake(reply);
      this.flush();
      if (mode === "enroll") {
        const final = await this.#session.writeHandshake();
        // From here the bridge may enroll this key; the caller keeps the
        // invitation until it sees `enrolled` or the recovery attempt fails.
        await onFinal?.();
        this.#send(final);
      }
    };
    let timer;
    await Promise.race([
      handshake(),
      new Promise((_, reject) => (timer = setTimeout(() => reject(new RemoteError("refused")), TIMES.handshake))),
    ]).finally(() => clearTimeout(timer));
    this.#phase = "transport";
    for (const data of this.#pending.splice(0)) this.#decrypt(data);
  }

  #message(event) {
    if (this.closed) return;
    if (typeof event.data === "string") return this.close("interrupted");
    const bytes = new Uint8Array(event.data);
    let e;
    try {
      e = unwrap(bytes);
    } catch {
      return this.close("interrupted");
    }
    if (this.#phase === "admission") {
      if (e.kind !== "open") return this.close("unavailable");
      this.#id = e.session;
      this.#phase = "handshake";
      return this.#notify();
    }
    if (e.session !== this.#id) return this.close("interrupted");
    if (e.kind === "close" || e.kind === "error")
      return this.close(this.#phase === "transport" ? "interrupted" : "refused");
    if (e.kind !== "data") return this.close("interrupted");
    this.#received += BigInt(bytes.length);
    if (this.#received - this.#acked >= BigInt(ACK_EVERY)) this.flush();
    if (this.#phase === "transport") return this.#decrypt(e.data);
    // The first data message is the bridge's handshake reply; later ones are
    // transport, held until our final handshake message is queued.
    if (this.#handshake === null) {
      this.#handshake = e.data;
      return this.#notify();
    }
    if (this.#pending.length >= 16) return this.close("interrupted");
    this.#pending.push(e.data);
  }

  #decrypt(data) {
    this.#chain = this.#chain
      .then(async () => {
        if (this.closed) return;
        const message = decode(await this.#session.decrypt(data));
        if (message.type !== "chunk") this.flush();
        const exchange = this.#exchanges.get(message.id);
        if (exchange) exchange.push(message);
        else if (this.#unrouted.length < 4) {
          this.#unrouted.push(message);
          this.#notify();
        }
      })
      .catch(() => this.close("interrupted"));
  }

  // The next message for no open request, such as `enrolled`.
  nextUnrouted(ms) {
    return this.#until(() => this.#unrouted.shift(), ms, "refused");
  }

  #send(bytes) {
    if (this.closed) fail(this.reason);
    this.#ws.send(envelope("data", this.#id, bytes));
  }

  // Relay receipt: at least every 64 KiB, and at the end of each burst.
  flush() {
    if (this.closed || this.#received === this.#acked || !this.#id) return;
    try {
      this.#ws.send(ack(this.#id, this.#received));
      this.#acked = this.#received;
    } catch {
      this.close("interrupted");
    }
  }

  async send(message) {
    const sealed = await this.#session.encrypt(encode(message));
    this.#send(sealed);
  }

  // Waits while the browser holds more than 1 MiB unsent.
  async drain() {
    while (!this.closed && this.#ws.bufferedAmount > SEND_BUFFER) await sleep(25);
    if (this.closed) fail(this.reason);
  }

  // Messages for one request ID, in order, with a deadline per wait.
  exchange(id) {
    const queue = [];
    const exchange = {
      push: (m) => {
        queue.push(m);
        this.#notify();
      },
      poll: () => queue.shift() ?? null,
      next: (ms) => this.#until(() => queue.shift(), ms, "timeout"),
      close: () => this.#exchanges.delete(id),
    };
    this.#exchanges.set(id, exchange);
    return exchange;
  }

  // Encrypted ping only while an operation is outstanding; a late pong ends
  // the link. Pings never count as transfer progress.
  async ping(timeout = TIMES.pong) {
    const id = randomId();
    const x = this.exchange(id);
    try {
      await this.send({ type: "ping", id });
      const m = await x.next(timeout);
      if (m.type !== "pong") fail("interrupted");
    } catch (error) {
      this.close("interrupted");
      throw error;
    } finally {
      x.close();
    }
  }

  keepAlive() {
    let timer;
    let stopped = false;
    const tick = () => {
      timer = setTimeout(async () => {
        if (stopped || this.closed) return;
        try {
          await this.ping();
          tick();
        } catch {}
      }, TIMES.ping);
    };
    tick();
    return () => {
      stopped = true;
      clearTimeout(timer);
    };
  }

  onClose(fn) {
    this.#closers.add(fn);
    return () => this.#closers.delete(fn);
  }

  close(reason = "closed") {
    if (this.closed) return;
    this.flush();
    this.closed = true;
    this.reason = reason;
    this.#notify();
    try {
      this.#ws?.close(1000);
    } catch {}
    for (const fn of this.#closers) fn(reason);
  }
}

function refusal(m) {
  if (m.type !== "error") fail("interrupted");
  fail(m.code === "store" ? "store" : "refused");
}

export async function list(link, cursor = "") {
  const id = randomId();
  const x = link.exchange(id);
  try {
    await link.send({ type: "list", id, cursor, limit: 100 });
    const m = await x.next(TIMES.response);
    if (m.type !== "list-page") refusal(m);
    return m;
  } finally {
    x.close();
  }
}

// Returns the whole file as a Blob only after a matching end record.
export async function get(link, key, { onProgress = () => {} } = {}) {
  const id = randomId();
  const x = link.exchange(id);
  try {
    await link.send({ type: "get", id, key });
    const head = await x.next(TIMES.response);
    if (head.type !== "get-head") refusal(head);
    const transfer = new Transfer(id, head.length);
    let granted = 0;
    let count = 0;
    let parts = [];
    let blob = new Blob([]);
    let pending = 0;
    const grant = async (n) => {
      link.flush();
      granted += n;
      await link.send({ type: "credit", id, chunks: n });
    };
    if (head.length > 0) await grant(WINDOW);
    for (;;) {
      const m = await x.next(TIMES.progress);
      if (m.type === "chunk") {
        try {
          transfer.accept(m);
        } catch (error) {
          fail("interrupted", error);
        }
        count++;
        parts.push(m.data);
        pending += m.data.length;
        // Hand large downloads to the Blob store in steps, not as one array.
        if (pending >= 4 * 1024 * 1024) {
          blob = new Blob([blob, ...parts]);
          parts = [];
          pending = 0;
        }
        onProgress(transfer.received / head.length);
        if (granted - count <= TOP_UP && transfer.received < head.length) await grant(TOP_UP);
      } else if (m.type === "get-end") {
        try {
          transfer.accept(m);
        } catch (error) {
          fail("interrupted", error);
        }
        return { blob: new Blob([blob, ...parts]), mime: head.mime, length: head.length };
      } else refusal(m);
    }
  } finally {
    x.close();
  }
}

// One upload under one request ID that belongs to exactly this File.
// `onBegin` runs just before put-begin leaves the page; after that only
// put-status can say what happened.
export async function put(link, upload, { onBegin = () => {}, onProgress = () => {}, onEnd = () => {} } = {}) {
  const { id, file } = upload;
  const x = link.exchange(id);
  try {
    const length = file.size;
    const begin = { type: "put-begin", id, name: uploadName(file.name), mime: uploadType(file.type), length };
    encode(begin); // a schema refusal here means nothing was sent
    await onBegin();
    await link.send(begin);
    let credits = 0;
    const take = (m) => {
      if (m.type !== "credit") refusal(m);
      credits += m.chunks;
      upload.accepted = true;
    };
    take(await x.next(TIMES.response));
    let at = 0;
    let index = 0;
    while (at < length) {
      for (let m = x.poll(); m; m = x.poll()) take(m);
      while (credits === 0) take(await x.next(TIMES.progress));
      const end = Math.min(at + CHUNK, length);
      let data;
      try {
        data = new Uint8Array(await file.slice(at, end).arrayBuffer());
      } catch (error) {
        link.close("interrupted");
        fail("unreadable", error);
      }
      if (data.length !== end - at) {
        link.close("interrupted");
        fail("unreadable");
      }
      await link.drain();
      await link.send({ type: "chunk", id, index, data });
      credits--;
      index++;
      at = end;
      onProgress(at / Math.max(length, 1));
    }
    await link.send({ type: "put-end", id, length, count: index });
    onEnd();
    for (;;) {
      const m = await x.next(TIMES.response);
      if (m.type === "put-done") return { key: m.key };
      if (m.type !== "credit") refusal(m);
    }
  } finally {
    x.close();
  }
}

export async function status(link, id) {
  const x = link.exchange(id);
  try {
    await link.send({ type: "put-status", id });
    const m = await x.next(TIMES.response);
    if (!["pending", "committed", "failed", "unknown"].includes(m.type)) refusal(m);
    return m;
  } finally {
    x.close();
  }
}

// The phone's view of its pairing and its one connection. `store` keeps the
// records (IndexedDB in the page) and `keys` creates and restores the device
// key (wrapped with a stored AES-GCM key in the page).
export class Remote {
  constructor({ origin, store, keys, WebSocketImpl, onStatus = () => {}, onRemoved = () => {} }) {
    Object.assign(this, { origin, store, keys, WebSocketImpl, onStatus, onRemoved });
    this.device = null;
    this.link = null;
    this.connecting = null;
    this.queue = new WorkQueue((work, item) => this.#perform(work, item));
    this.idleTimer = null;
    this.generation = 0;
  }

  url(route) {
    const u = new URL(`/${route}`, this.origin);
    u.protocol = u.protocol === "https:" ? "wss:" : "ws:";
    return u.href;
  }

  async load() {
    this.device = (await this.store.get("device")) ?? null;
    return this.device;
  }

  status(state, reason) {
    this.onStatus(state, reason);
  }

  // Pairs with an invitation. An existing pairing stays until this one is
  // acknowledged; on any failure the page is left as it was.
  async pair(invite, { onStep = () => {} } = {}) {
    const generation = this.generation;
    onStep("connecting");
    const identity = await this.keys.create();
    const pending = {
      route: invite.route,
      bridge: invite.bridge,
      invitation: invite.invitation,
      token: invite.token,
      publicKey: identity.publicKey,
      wrapped: identity.wrapped,
      iv: identity.iv,
      created: Date.now(),
    };
    let finalSent = false;
    let link;
    try {
      link = await Link.open({
        url: this.url(invite.route),
        WebSocketImpl: this.WebSocketImpl,
        token: invite.token,
        onAdmitted: () => onStep("checking"),
        mode: "enroll",
        route: invite.route,
        privateKey: identity.privateKey,
        publicKey: identity.publicKey,
        bridge: invite.bridge,
        psk: invite.psk,
        onFinal: async () => {
          await this.store.put("pending", pending);
          finalSent = true;
          onStep("saving");
        },
      });
      return await this.#enrolled(link, pending, generation);
    } catch (error) {
      link?.close();
      if (!finalSent || error.reason === "forgotten") throw this.#pairingError(error);
      // The bridge may have enrolled this key and lost the acknowledgement:
      // ask again on the invitation token with the same key, once.
      try {
        return await this.recoverPairing(identity.privateKey, generation);
      } catch (second) {
        await this.store.del("pending");
        throw this.#pairingError(second, "unfinished");
      }
    }
  }

  #pairingError(error, otherwise) {
    const reason = error?.reason;
    if (["unreachable", "unavailable", "home-offline", "busy", "unsupported"].includes(reason)) return new RemoteError(reason, error);
    if (reason === "cancelled" || reason === "forgotten") return new RemoteError(reason, error);
    return new RemoteError(otherwise ?? "code-refused", error);
  }

  async #enrolled(link, pending, generation) {
    // The acknowledgement is the bridge's first transport message.
    const first = await link.nextUnrouted(TIMES.handshake);
    if (first.type !== "enrolled") fail("refused");
    if (generation !== this.generation) {
      link.close();
      fail("forgotten");
    }
    const device = {
      route: pending.route,
      bridge: pending.bridge,
      id: first.id,
      token: first.token,
      publicKey: pending.publicKey,
      wrapped: pending.wrapped,
      iv: pending.iv,
      paired: Date.now(),
    };
    await this.store.put("device", device);
    await this.store.del("pending");
    this.device = device;
    this.#adopt(link);
    return device;
  }

  async recoverPairing(privateKey, generation = this.generation) {
    const pending = await this.store.get("pending");
    if (!pending) fail("unfinished");
    if (Date.now() - pending.created > 15 * 60 * 1000) {
      await this.store.del("pending");
      fail("unfinished");
    }
    privateKey ??= await this.keys.restore(pending);
    const link = await Link.open({
      url: this.url(pending.route),
      WebSocketImpl: this.WebSocketImpl,
      token: pending.token,
      mode: "reconnect",
      route: pending.route,
      privateKey,
      publicKey: pending.publicKey,
      bridge: pending.bridge,
    });
    try {
      return await this.#enrolled(link, pending, generation);
    } catch (error) {
      link.close();
      throw error;
    }
  }

  #replaceLink(link) {
    if (this.link && this.link !== link) this.link.close("closed");
    this.link = link;
  }

  #adopt(link) {
    this.#replaceLink(link);
    link.onClose(() => {
      if (this.link === link) {
        this.link = null;
        this.status("idle");
      }
    });
    this.status("connected");
    this.#touch();
  }

  #touch() {
    clearTimeout(this.idleTimer);
    this.idleTimer = setTimeout(() => {
      if (!this.queue.busy && !this.queue.length) this.disconnect();
    }, TIMES.idle);
    this.idleTimer?.unref?.();
  }

  // Connects with the device token, or returns the live link.
  async ensure() {
    if (this.link && !this.link.closed) return this.link;
    if (this.connecting) return this.connecting;
    const device = this.device;
    if (!device) fail("unpaired");
    const generation = this.generation;
    this.status("connecting");
    this.connecting = (async () => {
      const privateKey = await this.keys.restore(device);
      const open = () =>
        Link.open({
          url: this.url(device.route),
          WebSocketImpl: this.WebSocketImpl,
          token: device.token,
          mode: "reconnect",
          route: device.route,
          privateKey,
          publicKey: device.publicKey,
          bridge: device.bridge,
        });
      let link;
      try {
        link = await open();
      } catch (error) {
        if (error.reason !== "not-accepted") throw error;
        // Right after the bridge reconnects it re-registers tokens; look again
        // before telling the owner this phone was removed.
        await sleep(TIMES.retry);
        if (generation !== this.generation) fail("forgotten");
        try {
          link = await open();
        } catch (second) {
          if (second.reason === "not-accepted") fail("removed");
          throw second;
        }
      }
      if (generation !== this.generation) {
        link.close();
        fail("forgotten");
      }
      this.#adopt(link);
      return link;
    })();
    try {
      return await this.connecting;
    } catch (error) {
      const reason = error.reason === "refused" ? "unrecognized" : error.reason ?? "unavailable";
      // Stop queued work from dialling again; the next user action, a
      // "Try again" or the page becoming visible resumes it.
      this.queue.pause();
      this.status("offline", reason);
      if (reason === "removed" || reason === "unrecognized") {
        this.queue.clear();
        this.onRemoved(reason);
      }
      throw new RemoteError(reason, error);
    } finally {
      this.connecting = null;
    }
  }

  // Queued work: `work(link, item)`. Idempotent work is retried once on a
  // fresh connection if the link drops under it.
  run(priority, work, { retry = true } = {}) {
    return this.queue.add(priority, { work, retry });
  }

  async #perform({ work, retry }, item) {
    clearTimeout(this.idleTimer);
    for (let attempt = 0; ; attempt++) {
      const link = await this.ensure();
      const stop = link.keepAlive();
      try {
        return await work(link, item);
      } catch (error) {
        const dropped = ["interrupted", "timeout"].includes(error.reason) || link.closed;
        if (dropped) link.close("interrupted");
        if (!(retry && dropped && attempt === 0)) throw error;
      } finally {
        stop();
        this.#touch();
      }
    }
  }

  // Page hidden: an idle link is closed now rather than left to die silently.
  hidden() {
    this.hiddenAt = Date.now();
    if (!this.queue.busy && this.link) this.disconnect();
  }

  // Page visible again: if a transfer is running, prove the link is alive with
  // one ping; a dead link interrupts the transfer, which then recovers.
  async visible() {
    const away = Date.now() - (this.hiddenAt ?? Date.now());
    this.hiddenAt = null;
    const link = this.link;
    if (!link || link.closed) return;
    if (!this.queue.busy) {
      if (away > 1000) this.disconnect();
      return;
    }
    try {
      await link.ping(TIMES.check);
    } catch {
      link.close("interrupted");
    }
  }

  disconnect() {
    clearTimeout(this.idleTimer);
    const link = this.link;
    this.link = null;
    link?.close("closed");
    this.status("idle");
  }

  // Local cleanup only: closes the session, cancels queued work and deletes
  // every stored record. The device stays listed at home until revoked there.
  async forget() {
    this.generation++;
    this.queue.clear();
    this.disconnect();
    this.device = null;
    await this.store.destroy();
  }
}

export { CancelledError, PRIORITY };
