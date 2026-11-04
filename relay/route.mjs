import { Budget, EVENT_MS } from "./budget.mjs";
import {
  envelope,
  unwrap,
  readAdmission,
  bindingBytes,
} from "../web/protocol/wire.mjs";
import {
  routeLimits,
  bridgeLimits,
  hex,
  hash,
  verifyBridgeToken,
  rate,
  RELAY_WINDOW,
  BRIDGE_PING,
  BRIDGE_PONG,
} from "./limits.mjs";
const view = (b) => new DataView(b.buffer, b.byteOffset, b.byteLength);
// Close codes for a refused phone admission, so the page can tell the owner
// what happened. Nothing else about the route or its tokens is disclosed.
export const PHONE_CLOSE = Object.freeze({
  notAccepted: 4001,
  homeOffline: 4002,
  busy: 4003,
});
class Refused extends Error {
  constructor(code) {
    super("phone");
    this.code = code;
  }
}
export class Relay {
  constructor(ctx, env) {
    this.ctx = ctx;
    this.env = env;
    this.limits = routeLimits(env);
    this.phoneLedger = new Budget(ctx, this.limits);
    this.bridgeLedger = new Budget(
      ctx,
      bridgeLimits(env),
      "bridge-budget",
      "bridgeAccount",
      { messages: 16, bytes: 4096, failures: 1, writes: 8, durationMs: 100 },
    );
    this.ledger = this.phoneLedger;
    if (env.KILL_SWITCH === "false")
      ctx.setWebSocketAutoResponse(
        new WebSocketRequestResponsePair(BRIDGE_PING, BRIDGE_PONG),
      );
    else ctx.setWebSocketAutoResponse();
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
    s.serializeAttachment({
      ...a,
      ...(this.phoneLedger.used ? { account: this.phoneLedger.used } : {}),
      ...(this.bridgeLedger.used
        ? { bridgeAccount: this.bridgeLedger.used }
        : {}),
    });
  }
  async budget(bytes = 0, failure = false, extraWrites = 0) {
    if (this.ledger === this.phoneLedger) this.ledger.limits = this.limits;
    return this.ledger.charge({
      bytes,
      messages: 1,
      failures: failure ? 1 : 0,
      writes: extraWrites,
    });
  }
  run(f, bridgeWork = false) {
    return this.ctx.blockConcurrencyWhile(async () => {
      this.ledger = bridgeWork ? this.bridgeLedger : this.phoneLedger;
      const started = Date.now();
      this.ledger.eventDay = Math.floor(started / 86400000);
      if (this.ledger === this.phoneLedger) this.ledger.limits = this.limits;
      let charged = false;
      try {
        await this.ledger.load();

        if (
          !(await this.ledger.charge({
            durationMs: EVENT_MS,
          }))
        ) {
          for (const s of this.phones()) this.drop(s, true);
          return new Response(null, { status: 429 });
        }
        charged = true;
        return await f();
      } finally {
        // Date.now is frozen between I/O. Finish with a real timer event,
        // including its short wait in the active duration, before going idle.
        if (charged) {
          if (this.ctx.finishClock) await this.ctx.finishClock();
          else await new Promise((resolve) => setTimeout(resolve, 1));
          this.ledger.finish(started, Date.now());
        }
        delete this.ledger.eventDay;
        this.ledger = this.phoneLedger;
      }
    });
  }
  async failAdmission(s, code) {
    await this.budget(0, !s.deserializeAttachment()?.headerFailure);
    this.drop(s, false, code);
  }
  drop(s, error = false, code) {
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
      s.close(code ?? (error ? 1008 : 1000));
    } catch {}
  }
  fetch(request) {
    const bridgeUpgrade = request.headers.get("X-Homepool-Bridge") === "1";
    const headerFailure =
      !bridgeUpgrade && request.headers.get("X-Homepool-Auth-Failed") === "1";
    return this.run(async () => {
      if (this.env.KILL_SWITCH !== "false") {
        for (const s of this.sockets()) this.drop(s);
        return new Response(null, { status: 503 });
      }
      if (
        bridgeUpgrade &&
        (this.bridge() ||
          this.sockets().some((s) => s.deserializeAttachment()?.bridgeUpgrade))
      )
        return new Response(null, { status: 409 });
      const b = this.ledger.used;
      if (
        b?.day === Math.floor(Date.now() / 86400000) &&
        !bridgeUpgrade &&
        b.failures >= this.limits.dailyFailures
      )
        return new Response(null, { status: 429 });
      if (headerFailure && !(await this.budget(0, true)))
        return new Response(null, { status: 429 });
      if (!(await this.ledger.charge({ messages: 20 })))
        return new Response(null, { status: 429 });
      if (
        !bridgeUpgrade &&
        this.sockets().filter(
          (s) => s.deserializeAttachment().role === "pending",
        ).length >= this.limits.pending
      ) {
        await this.budget(0, !headerFailure);
        return new Response(null, { status: 429 });
      }
      const [client, server] = Object.values(new WebSocketPair());
      server.binaryType = "arraybuffer";
      this.ctx.acceptWebSocket(server);
      this.save(server, {
        role: "pending",
        deadline: Date.now() + this.limits.admissionMs,
        route: new URL(request.url).pathname.slice(1),
        bridgeUpgrade,
        headerFailure,
      });
      await this.schedule();
      return new Response(null, { status: 101, webSocket: client });
    }, bridgeUpgrade);
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
    // Lifecycle cleanup must run even when forwarding budgets are spent.
    // Its finite socket/grant bound is included in the auxiliary allowance.
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
    const attached = s.deserializeAttachment();
    const bridgeWork =
      attached?.bridgeUpgrade ||
      (attached?.role === "bridge" &&
        message instanceof ArrayBuffer &&
        message.byteLength >= 6 &&
        new DataView(message).getUint32(2) === 0);
    // A full receipt for an already retired session releases existing state
    // once. Permit this bounded cleanup even after either daily guard stops
    // forwarding, so lost ACKs cannot permanently occupy next day's slots.
    if (
      attached?.role === "bridge" &&
      message instanceof ArrayBuffer &&
      message.byteLength === 14
    ) {
      const frame = new Uint8Array(message);
      const id = new DataView(message).getUint32(2);
      if (frame[0] === 1 && frame[1] === 8 && attached.retired?.[id]) {
        return this.ctx.blockConcurrencyWhile(async () => {
          const a = s.deserializeAttachment();
          const f = a.retired?.[id];
          if (f && new DataView(message).getBigUint64(6) === BigInt(f.sent)) {
            delete a.retired[id];
            this.save(s, a);
          }
        });
      }
    }
    return this.run(async () => {
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
        // Control writes are charged only when token metadata actually changes.
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
      } catch (e) {
        if (a.role === "pending") await this.failAdmission(s, e?.code);
        else if (
          a.role === "bridge" &&
          (!bridgeWork || e?.message === "budget")
        ) {
          for (const p of this.phones()) this.drop(p, true);
        } else this.drop(s, true);
      }
    }, bridgeWork);
  }
  async admit(s, a, b) {
    if (Date.now() >= a.deadline) throw Error("deadline");
    const m = readAdmission(b);
    if (m.role === "bridge") {
      if (
        !(await verifyBridgeToken(this.env, a.route, b.slice(2))) ||
        this.bridge()
      )
        throw Error("bridge");
      this.save(s, { role: "bridge", next: 1 });
      return;
    }
    // Home first: while the bridge is away its token registrations may be
    // stale, so an unknown token is reported only to a connected bridge's route.
    const bridge = this.bridge();
    if (!bridge) throw new Refused(PHONE_CLOSE.homeOffline);
    const tokens = (await this.ctx.storage.get("tokens")) ?? {};
    const t = tokens[await hash(b.slice(2))];
    if (
      !t ||
      (t.expiry !== "0" &&
        BigInt(t.expiry) <= BigInt(Math.floor(Date.now() / 1000)))
    )
      throw new Refused(PHONE_CLOSE.notAccepted);
    if (
      this.phones().length +
        Object.keys(bridge.deserializeAttachment().retired ?? {}).length >=
      this.limits.sessions
    )
      throw new Refused(PHONE_CLOSE.busy);
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
    const tokens = (await this.ctx.storage.get("tokens")) ?? {};
    const before = JSON.stringify(tokens);
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
    if (JSON.stringify(tokens) === before) return;
    if (!(await this.ledger.charge({ writes: 1 }))) throw Error("budget");
    await this.ctx.storage.put("tokens", tokens);
  }
}
