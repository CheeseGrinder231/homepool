import {
  envelope,
  unwrap,
  readAdmission,
  bindingBytes,
} from "../web/protocol/wire.mjs";
import {
  defaults,
  hex,
  hash,
  constantTimeToken,
  rate,
  RELAY_WINDOW,
} from "./limits.mjs";
const tokenBytes = (s) =>
  /^[0-9a-f]{64}$/.test(s ?? "")
    ? Uint8Array.from(s.match(/../g), (x) => parseInt(x, 16))
    : null;
const view = (b) => new DataView(b.buffer, b.byteOffset, b.byteLength);
export class Relay {
  constructor(ctx, env) {
    this.ctx = ctx;
    this.env = env;
    this.limits = defaults;
  }
  sockets() {
    return this.ctx
      .getWebSockets()
      .filter((s) => s.readyState === 1 && !s.deserializeAttachment()?.closed);
  }
  bridge() {
    return this.sockets().find(
      (s) => s.deserializeAttachment()?.role === "bridge",
    );
  }
  phones() {
    return this.sockets().filter(
      (s) => s.deserializeAttachment()?.role === "phone",
    );
  }
  save(s, a) {
    s.serializeAttachment(a);
  }
  async budget(bytes = 0, failure = false) {
    const day = Math.floor(Date.now() / 86400000);
    let b = await this.ctx.storage.get("budget");
    if (b?.day !== day) b = { day, bytes: 0, messages: 0, failures: 0 };
    const ok =
      b.bytes + bytes <= this.limits.dailyBytes &&
      b.messages + 1 <= this.limits.dailyMessages &&
      b.failures + (failure ? 1 : 0) <= this.limits.dailyFailures;
    if (ok) {
      b.bytes += bytes;
      b.messages++;
      if (failure) b.failures++;
      await this.ctx.storage.put("budget", b);
    }
    return ok;
  }
  async failAdmission(s) {
    await this.budget(0, true);
    this.drop(s);
  }
  drop(s, error = false) {
    const a = s.deserializeAttachment();
    if (!a || a.closed) return;
    a.closed = true;
    this.save(s, a);
    if (a.role === "bridge") {
      for (const p of this.phones()) this.drop(p, true);
    } else if (a.role === "phone") {
      const b = this.bridge();
      if (b && BigInt(a.up.sent) > BigInt(a.up.acked)) {
        const ba = b.deserializeAttachment();
        ba.retired ??= {};
        ba.retired[a.id] = a.up;
        this.save(b, ba);
      }
      try {
        b?.send(envelope(error ? "error" : "close", a.id));
      } catch {
        if (b) this.drop(b);
      }
      if (error) {
        try {
          s.send(envelope("error", a.id));
        } catch {}
      }
    }
    try {
      s.close(error ? 1008 : 1000);
    } catch {}
  }
  fetch(request) {
    return this.ctx.blockConcurrencyWhile(async () => {
      if (this.env.KILL_SWITCH !== "false") {
        for (const s of this.sockets()) this.drop(s);
        return new Response(null, { status: 503 });
      }
      const b = await this.ctx.storage.get("budget");
      if (
        b?.day === Math.floor(Date.now() / 86400000) &&
        b.failures >= this.limits.dailyFailures
      )
        return new Response(null, { status: 429 });
      if (!(await this.budget())) return new Response(null, { status: 429 });
      if (
        this.sockets().filter(
          (s) => s.deserializeAttachment().role === "pending",
        ).length >= this.limits.pending
      ) {
        await this.budget(0, true);
        return new Response(null, { status: 429 });
      }
      const [client, server] = Object.values(new WebSocketPair());
      server.binaryType = "arraybuffer";
      this.ctx.acceptWebSocket(server);
      this.save(server, {
        role: "pending",
        deadline: Date.now() + this.limits.admissionMs,
        route: new URL(request.url).pathname.slice(1),
      });
      await this.schedule();
      return new Response(null, { status: 101, webSocket: client });
    });
  }
  async schedule() {
    let next = Infinity;
    for (const s of this.sockets()) {
      const a = s.deserializeAttachment();
      if (a.role === "pending") next = Math.min(next, a.deadline);
      if (a.role === "phone" && a.expiry !== "0") {
        const ms = BigInt(a.expiry) * 1000n;
        if (ms <= BigInt(Number.MAX_SAFE_INTEGER))
          next = Math.min(next, Number(ms));
      }
    }
    if (Number.isFinite(next))
      await this.ctx.storage.setAlarm(Math.max(Date.now() + 1, next));
    else await this.ctx.storage.deleteAlarm();
  }
  alarm() {
    return this.ctx.blockConcurrencyWhile(async () => {
      for (const s of this.sockets()) {
        const a = s.deserializeAttachment();
        if (this.env.KILL_SWITCH !== "false") this.drop(s);
        else if (a.role === "pending" && a.deadline <= Date.now())
          await this.failAdmission(s);
        else if (
          a.role === "phone" &&
          a.expiry !== "0" &&
          BigInt(a.expiry) <= BigInt(Math.floor(Date.now() / 1000))
        )
          this.drop(s, true);
      }
      await this.schedule();
    });
  }
  webSocketClose(s) {
    return this.ctx.blockConcurrencyWhile(async () => {
      this.drop(s);
      await this.schedule();
    });
  }
  webSocketError(s) {
    return this.webSocketClose(s);
  }
  webSocketMessage(s, message) {
    return this.ctx.blockConcurrencyWhile(async () => {
      const a = s.deserializeAttachment();
      if (!a || a.closed) return;
      try {
        if (this.env.KILL_SWITCH !== "false") {
          for (const p of this.sockets()) this.drop(p);
          return;
        }
        if (!(message instanceof ArrayBuffer) || message.byteLength > 65541)
          throw Error("frame");
        const b = new Uint8Array(message);
        if (!(await this.budget(b.length))) throw Error("budget");
        if (a.role === "pending") {
          await this.admit(s, a, b);
          await this.schedule();
          return;
        }
        const e = unwrap(b);
        if (!rate(a, Date.now(), b.length, e.kind !== "data", this.limits))
          throw Error("rate");
        this.save(s, a);
        if (a.role === "bridge" && e.session === 0) {
          await this.control(s, e);
          return;
        }
        if (a.role === "phone" && e.session !== a.id) throw Error("session");
        const p =
          a.role === "phone"
            ? s
            : this.phones().find(
                (p) => p.deserializeAttachment().id === e.session,
              );
        if (!p) {
          // A closed session holds its slot while bridge output is outstanding.
          // Only its bounded existing reservation can be ACKed; never allocate.
          if (
            a.role === "bridge" &&
            e.kind === "ack" &&
            a.retired?.[e.session]
          ) {
            const f = a.retired[e.session],
              n = view(e.data).getBigUint64(0);
            if (n < BigInt(f.acked) || n > BigInt(f.sent)) throw Error("ack");
            f.acked = n.toString();
            if (f.acked === f.sent) delete a.retired[e.session];
            this.save(s, a);
          }
          return;
        }
        const state = p.deserializeAttachment();
        if (
          state.expiry !== "0" &&
          BigInt(state.expiry) <= BigInt(Math.floor(Date.now() / 1000))
        ) {
          this.drop(p, true);
          return;
        }
        if (e.kind === "ack") {
          const direction = a.role === "phone" ? "down" : "up",
            n = view(e.data).getBigUint64(0),
            f = state[direction];
          if (n < BigInt(f.acked) || n > BigInt(f.sent)) {
            this.drop(p, true);
            return;
          }
          f.acked = n.toString();
          this.save(p, state);
          return;
        }
        if (e.kind === "close" || e.kind === "error") {
          this.drop(p, e.kind === "error");
          return;
        }
        if (e.kind !== "data") throw Error("direction");
        const target = a.role === "phone" ? this.bridge() : p;
        if (!target) {
          this.drop(p, true);
          return;
        }
        const f = state[a.role === "phone" ? "up" : "down"];
        const next = BigInt(f.sent) + BigInt(b.length);
        // Reserve before send. Never enqueue data that would cross the bound.
        if (
          next - BigInt(f.acked) > BigInt(RELAY_WINDOW) ||
          next > 0xffffffffffffffffn
        ) {
          this.drop(p, true);
          return;
        }
        f.sent = next.toString();
        this.save(p, state);
        target.send(b);
      } catch {
        if (a.role === "pending") await this.failAdmission(s);
        else this.drop(s, true);
      }
    });
  }
  async admit(s, a, b) {
    if (Date.now() >= a.deadline) throw Error("deadline");
    const m = readAdmission(b);
    if (m.role === "bridge") {
      let secret;
      try {
        secret = tokenBytes(JSON.parse(this.env.BRIDGE_TOKENS)[a.route]);
      } catch {}
      if (
        !secret ||
        !(await constantTimeToken(b.slice(2), secret)) ||
        this.bridge()
      )
        throw Error("bridge");
      this.save(s, { role: "bridge", next: 1 });
      return;
    }
    const bridge = this.bridge();
    const tokens = (await this.ctx.storage.get("tokens")) ?? {};
    const t = tokens[await hash(b.slice(2))];
    if (
      !bridge ||
      !t ||
      (t.expiry !== "0" &&
        BigInt(t.expiry) <= BigInt(Math.floor(Date.now() / 1000))) ||
      this.phones().length +
        Object.keys(bridge?.deserializeAttachment().retired ?? {}).length >=
        this.limits.sessions
    )
      throw Error("phone");
    const ba = bridge.deserializeAttachment();
    if (ba.next > 0xffffffff) throw Error("ids");
    const id = ba.next++;
    this.save(bridge, ba);
    this.save(s, {
      role: "phone",
      id,
      binding: t.binding,
      expiry: t.expiry,
      up: { sent: "0", acked: "0" },
      down: { sent: "0", acked: "0" },
    });
    const opened = envelope("open", id, bindingBytes(t.binding));
    bridge.send(opened);
    s.send(opened);
  }
  async control(s, e) {
    if (e.kind === "ping") {
      s.send(envelope("pong", 0, e.data));
      return;
    }
    if (e.kind === "pong") return;
    const tokens = (await this.ctx.storage.get("tokens")) ?? {};
    if (e.kind === "token-set") {
      const binding = view(e.data).getUint32(33),
        key = hex(e.data.slice(0, 32));
      // One current hash per binding; replacing it revokes the old admissions.
      if (
        tokens[key] &&
        (tokens[key].binding !== binding ||
          tokens[key].expiry !== view(e.data).getBigUint64(37).toString() ||
          tokens[key].tokenClass !== e.data[32])
      ) {
        const oldBinding = tokens[key].binding;
        for (const p of this.phones())
          if (p.deserializeAttachment().binding === oldBinding)
            this.drop(p, true);
      }
      const existing = Object.entries(tokens).find(
        ([h, t]) => t.binding === binding && h !== key,
      );
      if (existing) {
        delete tokens[existing[0]];
        for (const p of this.phones())
          if (p.deserializeAttachment().binding === binding) this.drop(p, true);
      }
      if (!tokens[key] && Object.keys(tokens).length >= this.limits.tokens)
        throw Error("tokens");
      tokens[key] = {
        binding,
        tokenClass: e.data[32],
        expiry: view(e.data).getBigUint64(37).toString(),
      };
    } else if (e.kind === "token-remove") {
      const binding = view(e.data).getUint32(0);
      for (const [h, t] of Object.entries(tokens))
        if (t.binding === binding) delete tokens[h];
      for (const p of this.phones())
        if (p.deserializeAttachment().binding === binding) this.drop(p, true);
    } else throw Error("control");
    await this.ctx.storage.put("tokens", tokens);
  }
}
