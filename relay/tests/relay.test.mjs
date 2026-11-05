import { test } from "node:test";
import assert from "node:assert/strict";
import { harness, Socket } from "./harness.mjs";
import {
  admission,
  tokenSet,
  envelope,
  unwrap,
  ack,
  bindingBytes,
} from "../../web/protocol/wire.mjs";
import {
  hash,
  RELAY_WINDOW,
  APPLICATION_WINDOW,
  BRIDGE_PING,
  BRIDGE_PONG,
} from "../limits.mjs";
import worker, { UpgradeGate } from "../worker.mjs";
const unhex = (s) => new Uint8Array(Buffer.from(s, "hex"));
const phoneToken = "cd".repeat(32);
async function bridge(h) {
  const s = h.pending();
  await h.send(s, admission("bridge", "ab".repeat(32)));
  return s;
}
async function register(h, b, expiry = 0n, binding = 42) {
  await h.send(
    b,
    tokenSet(unhex(await hash(unhex(phoneToken))), 1, binding, expiry),
  );
}
async function phone(h) {
  const p = h.pending();
  await h.send(p, admission("phone", phoneToken));
  return p;
}
const data = (id, n = 64) => envelope("data", id, new Uint8Array(n));

test("admission, binding, revocation, no forwarding before admission, bridge cannot be evicted", async () => {
  const h = harness(),
    b = await bridge(h);
  assert.equal(b.data.role, "bridge");
  const wrong = h.pending();
  await h.send(wrong, admission("bridge", "00".repeat(32)));
  assert.equal(wrong.readyState, 3);
  const second = await bridge(h);
  assert.equal(second.readyState, 3);
  assert.equal(b.readyState, 1);
  const early = h.pending();
  await h.send(early, data(1));
  assert.equal(early.readyState, 3);
  assert.equal(b.sent.length, 0);
  assert.equal((await phone(h)).readyState, 3);
  await register(h, b);
  const p = await phone(h);
  assert.equal(p.data.binding, 42);
  assert.equal(
    new DataView(unwrap(b.sent.at(-1)).data.buffer).getUint32(0),
    42,
  );
  await h.send(p, data(p.data.id));
  assert.equal(unwrap(b.sent.at(-1)).kind, "data");
  await h.send(b, envelope("token-remove", 0, bindingBytes(42)));
  assert.equal(p.readyState, 3);
  assert.equal((await phone(h)).readyState, 3);
  await register(h, b, BigInt(Math.floor(Date.now() / 1000)) - 1n);
  assert.equal((await phone(h)).readyState, 3);
  assert.equal(b.readyState, 1);
});
test("token control privilege and phone routing cannot be forged", async () => {
  const h = harness(),
    b = await bridge(h);
  await register(h, b);
  const p = await phone(h);
  await h.send(p, tokenSet(new Uint8Array(32), 1, 1));
  assert.equal(p.readyState, 3);
  assert.equal(Object.keys(h.values.get("tokens")).length, 1);
  const q = await phone(h);
  await h.send(q, data(q.data.id + 1));
  assert.equal(q.readyState, 3);
});
test("fixed text auto-response is liveness only and survives reconstruction", async () => {
  const h = harness(),
    b = await bridge(h);
  h.restart();
  const before = structuredClone(h.stats);
  await h.send(b, BRIDGE_PING);
  assert.equal(b.sent.pop(), BRIDGE_PONG);
  assert.deepEqual(h.stats, before);
  const pending = h.pending();
  await h.send(pending, BRIDGE_PING);
  assert.equal(pending.sent.pop(), BRIDGE_PONG);
  assert.equal(pending.data.role, "pending");
  assert.equal(h.relay.bridge(), b);
  pending.data.deadline = Date.now() - 1;
  await h.relay.alarm();
  assert.equal(pending.readyState, 3);
});
test("slow receiver: hard window checked before send in both directions; ACK rejects false totals", async () => {
  for (const down of [false, true]) {
    const h = harness(),
      b = await bridge(h);
    await register(h, b);
    const p = await phone(h),
      id = p.data.id;
    const target = down ? p : b,
      source = down ? b : p;
    target.sent = [];
    let forwarded = 0;
    while (p.readyState === 1) {
      const m = data(id, 65535);
      await h.send(source, m);
      if (p.readyState === 1) forwarded += m.length;
    }
    const actual = target.sent
      .filter((x) => unwrap(x).kind === "data")
      .reduce((n, x) => n + x.length, 0);
    assert.equal(actual, forwarded);
    assert.ok(actual <= RELAY_WINDOW);
    assert.ok(actual + 65541 > RELAY_WINDOW);
    assert.equal(b.readyState, 1); // Session failure never evicts the shared bridge.
  }
  const h = harness(),
    b = await bridge(h);
  await register(h, b);
  const p = await phone(h);
  await h.send(p, ack(p.data.id, 1n));
  assert.equal(p.readyState, 3);
});
test("session cap recovers; oversize and per-socket byte/message/control rates close offenders", async () => {
  const h = harness({ sessions: 1 }),
    b = await bridge(h);
  await register(h, b);
  const p = await phone(h);
  assert.equal((await phone(h)).readyState, 3);
  h.relay.drop(p);
  assert.equal((await phone(h)).readyState, 1);
  for (const limits of [
    { bytesPerSecond: 10 },
    { messagesPerSecond: 1 },
    { controlsPerSecond: 1 },
    { controlBytesPerSecond: 10 },
  ]) {
    const x = harness(limits),
      c = await bridge(x);
    await register(x, c);
    if (c.readyState === 1) {
      await x.send(c, envelope("token-remove", 0, bindingBytes(999)));
      await x.send(c, envelope("token-remove", 0, bindingBytes(999)));
    }
    assert.equal(c.readyState, 3);
  }
  const x = harness(),
    c = await bridge(x);
  await x.send(c, new Uint8Array(65542));
  assert.equal(c.readyState, 3);
});
test("daily counters persist through reconstruction and reset at UTC day boundary", async () => {
  const now = Date.now;
  let clock = 864000000;
  Date.now = () => clock;
  try {
    for (const limits of [
      { dailyBytes: 80 },
      { dailyMessages: 2 },
      { dailyFailures: 1 },
    ]) {
      const h = harness(limits),
        b = await bridge(h);
      if (limits.dailyFailures) {
        const wrong = h.pending();
        await h.send(wrong, admission("bridge", "00".repeat(32)));
      } else await h.relay.budget(50);
      h.restart();
      const saved = structuredClone(h.values.get("budget"));
      assert.ok(saved);
      assert.deepEqual(await h.ctx.storage.get("budget"), saved);
      if (limits.dailyBytes) assert.equal(await h.relay.budget(80), false);
      if (limits.dailyMessages) assert.equal(await h.relay.budget(), false);
      if (limits.dailyFailures)
        assert.equal(await h.relay.budget(0, true), false);
      clock += 86400000;
      assert.equal(await h.relay.budget(1, true), true);
    }
  } finally {
    Date.now = now;
  }
});
test("alarms enforce admission deadline and admitted token expiry after hibernation", async () => {
  const h = harness(),
    b = await bridge(h),
    p = h.pending();
  p.data.deadline = Date.now() - 1;
  await register(h, b, BigInt(Math.floor(Date.now() / 1000)) + 60n);
  const q = await phone(h);
  q.data.expiry = "1";
  h.restart();
  await h.relay.alarm();
  assert.equal(p.readyState, 3);
  assert.equal(q.readyState, 3);
});
test("unknown routes allocate no objects; kill switch and invalid URLs reject before upgrade", async () => {
  let calls = 0;
  const ns = {
    idFromName() {
      calls++;
      return "id";
    },
  };
  const env = {
    KILL_SWITCH: "false",
    ROUTES: '["example"]',
    UPGRADES: ns,
    ROUTES_DO: ns,
  };
  for (const path of ["/unknown", "/example?token=x"])
    assert.equal(
      (await worker.fetch(new Request(new URL(path, import.meta.url)), env))
        .status,
      404,
    );
  env.ROUTES = JSON.stringify(Array.from({ length: 101 }, (_, n) => String(n)));
  assert.equal(
    (await worker.fetch(new Request(new URL("/0", import.meta.url)), env))
      .status,
    503,
  );
  env.ROUTES = '["example"]';
  env.KILL_SWITCH = "true";
  assert.equal(
    (await worker.fetch(new Request(new URL("/example", import.meta.url)), env))
      .status,
    503,
  );
  assert.equal(calls, 0);
  const h = harness(),
    b = await bridge(h);
  h.env.KILL_SWITCH = "true";
  await h.send(b, envelope("token-remove", 0, bindingBytes(999)));
  assert.equal(b.readyState, 3);
});
test("global rate survives new coordinator instances and recovers next second", async () => {
  const h = harness(),
    now = Date.now;
  let clock = 100000;
  Date.now = () => clock;
  try {
    let gate = new UpgradeGate(h.ctx, {});
    for (let i = 0; i < 20; i++) assert.equal((await gate.fetch()).status, 204);
    gate = new UpgradeGate(h.ctx, {});
    assert.equal((await gate.fetch()).status, 429);
    clock += 1000;
    assert.equal((await gate.fetch()).status, 204);
  } finally {
    Date.now = now;
  }
});
test("stale IDs allocate nothing and decreasing ACK closes only the session", async () => {
  const h = harness(),
    b = await bridge(h);
  await register(h, b);
  const p = await phone(h);
  await h.send(b, data(999));
  assert.equal(h.relay.phones().length, 1);
  await h.send(b, data(p.data.id));
  await h.send(p, ack(p.data.id, 70n));
  h.restart();
  await h.send(p, ack(p.data.id, 69n));
  assert.equal(p.readyState, 3);
  assert.equal(b.readyState, 1);
});

// Exercise real fetch logic with only the platform's upgrade objects substituted.
async function upgrade(h, headers = {}) {
  const ResponseOriginal = globalThis.Response,
    PairOriginal = globalThis.WebSocketPair;
  globalThis.WebSocketPair = class {
    constructor() {
      this[0] = new Socket();
      this[1] = new Socket();
    }
  };
  globalThis.Response = class extends ResponseOriginal {
    constructor(body, init = {}) {
      super(body, { ...init, status: init.status === 101 ? 200 : init.status });
      this.upgradeStatus = init.status;
      this.webSocket = init.webSocket;
    }
    get status() {
      return this.upgradeStatus ?? super.status;
    }
  };
  try {
    return await h.relay.fetch(
      new Request(new URL("/example", import.meta.url), { headers }),
    );
  } finally {
    globalThis.Response = ResponseOriginal;
    globalThis.WebSocketPair = PairOriginal;
  }
}
test("pending upgrade cap and failed admission budget trip and recover", async () => {
  const h = harness({ pending: 1 });
  assert.equal((await upgrade(h)).status, 101);
  assert.equal((await upgrade(h)).status, 429);
  const s = h.sockets.at(-1);
  s.data.deadline = Date.now() - 1;
  await h.relay.alarm();
  assert.equal((await upgrade(h)).status, 101);
  const x = harness({ dailyFailures: 1 });
  const wrong = x.pending();
  await x.send(wrong, admission("bridge", "00".repeat(32)));
  x.restart();
  assert.equal((await upgrade(x)).status, 429);
  const now = Date.now;
  Date.now = () => now() + 86400000;
  try {
    assert.equal((await upgrade(x)).status, 101);
  } finally {
    Date.now = now;
  }
});
test("daily byte and message caps reject data before forwarding and reset after reconstruction", async () => {
  for (const key of ["dailyBytes", "dailyMessages"]) {
    const h = harness(),
      b = await bridge(h);
    await register(h, b);
    const p = await phone(h),
      before = b.sent.length;
    const budget = h.relay.ledger.used;
    h.relay.limits = {
      ...h.relay.limits,
      [key]: key === "dailyBytes" ? budget.bytes : budget.messages,
    };
    await h.send(p, data(p.data.id));
    assert.equal(p.readyState, 3);
    assert.equal(b.sent.filter((x) => unwrap(x).kind === "data").length, 0);
    assert.ok(b.sent.length >= before);
    h.restart();
    const now = Date.now;
    Date.now = () => now() + 86400000;
    try {
      assert.equal(await h.relay.budget(), true);
    } finally {
      Date.now = now;
    }
  }
});
test("message, byte, and control rate windows reset on the next second", async () => {
  const now = Date.now;
  let clock = 100000;
  Date.now = () => clock;
  try {
    const h = harness({
        messagesPerSecond: 2,
        controlsPerSecond: 2,
        controlBytesPerSecond: 100,
      }),
      b = await bridge(h);
    await register(h, b);
    await h.send(b, envelope("token-remove", 0, bindingBytes(999)));
    clock += 1000;
    await h.send(b, envelope("token-remove", 0, bindingBytes(999)));
    await h.send(b, envelope("token-remove", 0, bindingBytes(999)));
    assert.equal(b.readyState, 1);
    await h.send(b, envelope("token-remove", 0, bindingBytes(999)));
    assert.equal(b.readyState, 3);
    clock += 1000;
    assert.equal((await bridge(h)).readyState, 1);
  } finally {
    Date.now = now;
  }
});
test("token registry cap, replacement revocation, and bridge loss", async () => {
  const h = harness({ tokens: 1 }),
    b = await bridge(h);
  await register(h, b);
  const p = await phone(h);
  await register(h, b, BigInt(Math.floor(Date.now() / 1000)) + 60n);
  assert.equal(p.readyState, 3);
  const q = await phone(h);
  await h.send(b, tokenSet(new Uint8Array(32), 1, 43));
  assert.equal(b.readyState, 3);
  assert.equal(q.readyState, 3);
  const c = await bridge(h);
  assert.equal(c.readyState, 1);
  await h.send(c, envelope("token-remove", 0, bindingBytes(42)));
  await h.send(c, tokenSet(new Uint8Array(32), 1, 43));
  assert.equal(c.readyState, 1);
});
test("window reservations and pending IDs survive attachment-only reconstruction", async () => {
  const h = harness(),
    b = await bridge(h);
  await register(h, b);
  const p = await phone(h);
  for (let i = 0; i < 14; i++) await h.send(p, data(p.data.id, 65535));
  h.restart();
  const before = b.sent.length;
  await h.send(p, data(p.data.id, 65535));
  assert.equal(b.sent.length, before + 1);
  await h.send(p, data(p.data.id, 65535));
  assert.equal(p.readyState, 3);
  assert.ok(
    b.sent
      .filter((x) => unwrap(x).kind === "data")
      .reduce((n, x) => n + x.length, 0) <= RELAY_WINDOW,
  );
});
test("concurrent bridge claims serialize: exactly one admitted, no eviction", async () => {
  const h = harness();
  const contenders = await Promise.all([bridge(h), bridge(h)]);
  assert.equal(contenders.filter((s) => s.readyState === 1).length, 1);
  assert.equal(h.relay.bridge(), contenders[0]);
});

test("closed sessions retain bridge reservations: churn cannot exceed aggregate cap", async () => {
  const h = harness({ sessions: 1 }),
    b = await bridge(h);
  await register(h, b);
  const p = await phone(h),
    id = p.data.id;
  await h.send(p, data(id, 65535));
  await h.send(p, envelope("close", id));
  assert.equal(p.readyState, 3);
  h.restart();
  const blocked = await phone(h);
  assert.equal(blocked.readyState, 3);
  assert.equal(Object.keys(b.data.retired).length, 1);
  await h.send(b, ack(id, 65541n));
  assert.equal(Object.keys(b.data.retired).length, 0);
  assert.equal((await phone(h)).readyState, 1);
});

test("exact window is allowed and an ACK replenishes it without reconnect", async () => {
  const h = harness(),
    b = await bridge(h);
  await register(h, b);
  const p = await phone(h),
    id = p.data.id;
  for (let i = 0; i < 16; i++) await h.send(p, data(id, 65530));
  assert.equal(p.readyState, 1);
  assert.equal(BigInt(p.data.up.sent), BigInt(RELAY_WINDOW));
  await h.send(b, ack(id, BigInt(RELAY_WINDOW)));
  await h.send(p, data(id, 65530));
  assert.equal(p.readyState, 1);
});

test("global daily upgrade cap survives reconstruction and resets at UTC midnight", async () => {
  const h = harness(),
    now = Date.now;
  let clock = 100000;
  Date.now = () => clock;
  try {
    for (let i = 0; i < defaults.dailyUpgrades; i++) {
      clock += 1000;
      assert.equal((await new UpgradeGate(h.ctx, {}).fetch()).status, 204);
    }
    clock += 1000;
    assert.equal((await new UpgradeGate(h.ctx, {}).fetch()).status, 429);
    clock += 86400000;
    assert.equal((await new UpgradeGate(h.ctx, {}).fetch()).status, 204);
  } finally {
    Date.now = now;
  }
});

test("bridge token write budget refuses mutation while preserving its connection", async () => {
  const h = harness(),
    b = await bridge(h);
  await register(h, b);
  h.relay.bridgeLedger.limits.dailyWrites = h.relay.bridgeLedger.used.writes;
  await h.send(b, envelope("token-remove", 0, bindingBytes(42)));
  assert.equal(b.readyState, 1);
  assert.equal(Object.keys(h.values.get("tokens")).length, 1);
  const now = Date.now;
  Date.now = () => now() + 86400000;
  try {
    await h.send(b, envelope("token-remove", 0, bindingBytes(42)));
    assert.equal(Object.keys(h.values.get("tokens")).length, 0);
  } finally {
    Date.now = now;
  }
});

import { EVENT_MS } from "../budget.mjs";
import {
  bridgeLimits,
  routeLimits,
  defaults,
  AUXILIARY_REQUESTS,
} from "../limits.mjs";
test("configured routes share Free-plan admitted-work budgets", () => {
  const one = routeLimits({ ROUTES: '["a"]' }),
    two = routeLimits({ ROUTES: '["a","b"]' });
  for (const k of [
    "dailyBytes",
    "dailyMessages",
    "dailyWrites",
    "dailyFailures",
    "dailyDurationMs",
  ])
    assert.equal(two[k] * 2, one[k]);
  assert.equal(one.dailyMessages, 570000);
  assert.equal(defaults.dailyUpgrades, 1000);
  assert.equal(defaults.dailyFailures, 100);
  assert.equal(defaults.maxRoutes, 100);
});

test("Free plan model includes independent bridge controls and auxiliary calls", () => {
  for (const count of [1, 2, 16, 100]) {
    const env = {
      ROUTES: JSON.stringify(
        Array.from({ length: count }, (_, n) => String(n)),
      ),
    };
    const phone = routeLimits(env),
      bridge = bridgeLimits(env);
    assert.ok(
      (count * (phone.dailyMessages + bridge.dailyMessages)) / 20 +
        (count * 5760) / 20 +
        AUXILIARY_REQUESTS <=
        50000,
    );
    const upgrades =
      defaults.dailyUpgrades +
      count *
        Math.min(
          defaults.bridgeUpgradesPerRoute,
          Math.floor(defaults.bridgeUpgrades / count),
        );
    assert.ok(
      (((phone.dailyDurationMs + bridge.dailyDurationMs) * count) / 1000) *
        0.128 +
        ((AUXILIARY_REQUESTS * EVENT_MS) / 1000) * 0.128 +
        (((2 * count + 1) * EVENT_MS) / 1000) * 0.128 <=
        6500,
    );
    assert.ok(
      (phone.dailyWrites + bridge.dailyWrites) * count + upgrades + 14200 <=
        50000,
    );
    assert.ok(
      (phone.dailyMessages + bridge.dailyMessages) * count * 2 +
        2 * AUXILIARY_REQUESTS +
        1000 <=
        2500000,
    );
    assert.ok(upgrades + 1000 * 14 <= 50000);
  }
});

test("HTTP upgrades charge a full request, while one binary message charges a twentieth", async () => {
  const h = harness();
  assert.equal((await upgrade(h)).status, 101);
  assert.equal(h.relay.ledger.used.messages, 20);
  await h.send(h.sockets.at(-1), admission("bridge", "ab".repeat(32)));
  assert.equal(h.relay.ledger.used.messages, 21);
});

test("full retired receipts release slots after both guards are exhausted", async () => {
  const h = harness(),
    b = await bridge(h);
  await register(h, b);
  const p = await phone(h);
  await h.send(p, data(p.data.id));
  const received = p.data.up.sent;
  h.relay.limits.dailyDurationMs = 0;
  await h.relay.run(async () => assert.fail("spent guard ran"));
  assert.ok(b.data.retired[p.data.id]);
  h.relay.bridgeLedger.limits.dailyDurationMs = 0;
  await h.send(b, ack(p.data.id, BigInt(received)));
  assert.deepEqual(b.data.retired, {});
  assert.equal(b.readyState, 1);
});

test("bad headers share unauthenticated admission and never spend bridge grants", async () => {
  const gates = new Map(),
    requests = [];
  let forwarded;
  const env = {
    KILL_SWITCH: "false",
    ROUTES: '["example"]',
    BRIDGE_TOKENS: JSON.stringify({ example: "ab".repeat(32) }),
    UPGRADES: {
      idFromName: (name) => name,
      get(name) {
        if (!gates.has(name)) {
          const h = harness();
          gates.set(name, { h, gate: new UpgradeGate(h.ctx, env) });
        }
        return {
          fetch: (r) => {
            requests.push({ name, request: r });
            return gates.get(name).gate.fetch(r);
          },
        };
      },
    },
    ROUTES_DO: {
      idFromName: (n) => n,
      get: () => ({
        fetch: async (r) => {
          forwarded = r;
          return new Response(null, { status: 200 });
        },
      }),
    },
  };
  const request = (headers = {}) =>
    new Request(new URL("/example", import.meta.url), {
      headers: {
        Upgrade: "websocket",
        "X-Homepool-Bridge": "1",
        "X-Homepool-Auth-Failed": "1",
        ...headers,
      },
    });
  for (const headers of [
    {},
    { Authorization: "Bearer " + "00".repeat(32) },
    { Authorization: "Bearer garbled" },
    { Authorization: "" },
    { Authorization: "Basic " + "ab".repeat(32) },
  ]) {
    assert.equal((await worker.fetch(request(headers), env)).status, 200);
    assert.equal(requests.at(-1).name, "global");
    assert.equal(
      requests.at(-1).request.headers.get("X-Homepool-Bridge"),
      null,
    );
    assert.equal(requests.at(-1).request.headers.get("Authorization"), null);
    assert.equal(
      requests.at(-1).request.headers.get("X-Homepool-Auth-Failed"),
      headers.Authorization === undefined ? null : "1",
    );
  }
  assert.equal(gates.size, 1);
  assert.equal(gates.get("global").h.values.get("rate").daily, 5);
  const global = gates.get("global").h;
  const rate = global.values.get("rate");
  rate.daily = defaults.dailyUpgrades;
  global.values.set("rate", rate);
  for (const headers of [{}, { Authorization: "Bearer garbled" }])
    assert.equal((await worker.fetch(request(headers), env)).status, 429);
  assert.equal(
    (
      await worker.fetch(
        request({ Authorization: "Bearer " + "ab".repeat(32) }),
        env,
      )
    ).status,
    200,
  );
  assert.equal(gates.size, 1);
  assert.equal(forwarded.headers.get("Authorization"), null);

  assert.equal(forwarded.headers.get("X-Homepool-Auth-Failed"), null);
});

test("a headerless upgrade still admits a legacy bridge with binary admission", async () => {
  const h = harness();
  assert.equal((await upgrade(h)).status, 101);
  const b = h.sockets.at(-1);
  assert.equal(b.data.bridgeUpgrade, false);
  await h.send(b, admission("bridge", "ab".repeat(32)));
  assert.equal(h.relay.bridge(), b);
});

test("exhausted guards cannot leave a closed bridge claiming its slot", async () => {
  const h = harness(),
    b = await bridge(h);
  h.relay.limits.dailyDurationMs = 0;
  await h.relay.webSocketClose(b);
  assert.equal(h.relay.bridge(), undefined);
  assert.equal(b.data.closed, true);
});

test("eight bridge reconnects with 128 registrations survive lost attachments at 100 routes", async () => {
  const h = harness();
  h.env.ROUTES = JSON.stringify(
    Array.from({ length: 100 }, (_, n) => String(n)),
  );
  h.restart();
  const realNow = Date.now;
  let clock = realNow();
  Date.now = () => clock;
  try {
    for (let reconnect = 0; reconnect < 8; reconnect++) {
      await h.relay.run(async () => {
        assert.equal(await h.relay.ledger.charge({ messages: 20 }), true);
      }, true);
      const b = h.pending();
      b.data.bridgeUpgrade = true;
      await h.send(b, admission("bridge", "ab".repeat(32)));
      assert.equal(b.data.role, "bridge");
      for (let n = 0; n < 128; n++) {
        const digest = new Uint8Array(32);
        new DataView(digest.buffer).setUint32(0, n);
        clock += 75; // Production bridge's registration cadence.
        await h.send(b, tokenSet(digest, 1, n));
        assert.equal(b.readyState, 1);
      }
      assert.equal(Object.keys(h.values.get("tokens")).length, 128);
      h.sockets.length = 0;
      h.restart();
    }
  } finally {
    Date.now = realNow;
  }
});

test("ten times the largest measured event remains within Free duration", () => {
  // Independent measured maximum, rounded up after applying 10x stress.
  const stressSeconds = Math.ceil((10 * 365) / 1000);
  for (const count of [1, 2, 16, 100]) {
    const guardedSeconds =
      18000 * Math.max(1, stressSeconds / (EVENT_MS / 1000));
    const lastEventOvershoot =
      2 * count * Math.max(0, stressSeconds - EVENT_MS / 1000);
    const seconds =
      guardedSeconds +
      14000 * stressSeconds +
      (2 * count + 1) * stressSeconds +
      lastEventOvershoot;
    assert.ok(seconds * 0.128 <= 13000);
  }
});

test("bridge admission compares against a cached configured hash without logging credentials", async () => {
  const { verifyBridgeToken } = await import("../limits.mjs");
  let reads = 0;
  const env = {
    get BRIDGE_TOKENS() {
      reads++;
      return JSON.stringify({ example: "ab".repeat(32) });
    },
  };
  assert.equal(
    await verifyBridgeToken(env, "example", unhex("ab".repeat(32))),
    true,
  );
  assert.equal(
    await verifyBridgeToken(env, "example", unhex("ac".repeat(32))),
    false,
  );
  assert.equal(
    await verifyBridgeToken(env, "example", unhex("ab".repeat(32))),
    true,
  );
  assert.equal(reads, 1);
  const original = {},
    logged = [];
  for (const level of ["log", "info", "warn", "error"]) {
    original[level] = console[level];
    console[level] = (...args) => logged.push(args);
  }
  try {
    assert.equal(
      await verifyBridgeToken(
        { BRIDGE_TOKENS: "synthetic malformed private binding" },
        "example",
        new Uint8Array(32),
      ),
      false,
    );
  } finally {
    Object.assign(console, original);
  }
  assert.deepEqual(logged, []);
});

test("invalid headers charge public failures once, even when the client closes immediately", async () => {
  const h = harness({ dailyFailures: 2 });
  assert.equal(
    (await upgrade(h, { "X-Homepool-Auth-Failed": "1" })).status,
    101,
  );
  assert.equal(h.relay.ledger.used.failures, 1);
  await h.relay.webSocketClose(h.sockets.at(-1));
  assert.equal(h.relay.ledger.used.failures, 1);
  assert.equal(
    (await upgrade(h, { "X-Homepool-Auth-Failed": "1" })).status,
    101,
  );
  await h.send(h.sockets.at(-1), admission("bridge", "00".repeat(32)));
  assert.equal(h.relay.ledger.used.failures, 2);
  assert.equal((await upgrade(h)).status, 429);
  assert.equal(h.relay.bridgeLedger.used, null);
});

test("authenticated replacement wins over both held and pending sockets", async () => {
  const h = harness();
  const headers = { "X-Homepool-Bridge": "1" };
  await upgrade(h, headers);
  const old = h.sockets.at(-1);
  await h.send(old, admission("bridge", "ab".repeat(32)));
  await upgrade(h, headers);
  const pending = h.sockets.at(-1);
  assert.equal(old.readyState, 3);
  assert.equal((await upgrade(h, headers)).status, 101);
  const current = h.sockets.at(-1);
  assert.equal(pending.readyState, 3);
  await h.send(current, admission("bridge", "ab".repeat(32)));
  await h.relay.webSocketClose(old);
  assert.equal(h.relay.bridge(), current);
});

test("refused bridge upgrades leave admitted allowance intact", async () => {
  const h = harness();
  h.env.KILL_SWITCH = "true";
  for (let n = 0; n < 12; n++)
    assert.equal((await upgrade(h, { "X-Homepool-Bridge": "1" })).status, 503);
  assert.equal(h.values.get("bridge-admissions"), undefined);
  h.env.KILL_SWITCH = "false";
  h.relay.bridgeLedger.limits.dailyMessages = 0;
  assert.equal((await upgrade(h, { "X-Homepool-Bridge": "1" })).status, 429);
  assert.equal(h.values.get("bridge-admissions"), undefined);
  h.relay.bridgeLedger.limits.dailyMessages = 130000;
  assert.equal((await upgrade(h, { "X-Homepool-Bridge": "1" })).status, 101);
  assert.equal(h.values.get("bridge-admissions").daily, 1);
});

test("a crowded deployment admits sixteen daily blips, survives reconstruction and resets", async () => {
  const h = harness();
  h.env.ROUTES = JSON.stringify(
    Array.from({ length: 100 }, (_, n) => String(n)),
  );
  const real = Date.now;
  let clock = 10 * 86400000;
  Date.now = () => clock;
  try {
    h.restart();
    for (let n = 0; n < 16; n++) {
      clock += 60000;
      assert.equal(
        (await upgrade(h, { "X-Homepool-Bridge": "1" })).status,
        101,
      );
      const b = h.sockets.at(-1);
      await h.send(b, admission("bridge", "ab".repeat(32)));
      assert.equal(h.relay.bridge(), b);
      for (let token = 0; token < 8; token++) {
        clock += 75;
        await h.send(b, tokenSet(new Uint8Array(32).fill(token), 1, token));
        assert.equal(b.readyState, 1);
      }
      h.sockets.length = 0;
      h.restart();
    }
    assert.equal((await upgrade(h, { "X-Homepool-Bridge": "1" })).status, 429);
    assert.equal(h.values.get("bridge-admissions").daily, 16);
    clock += 86400000;
    assert.equal((await upgrade(h, { "X-Homepool-Bridge": "1" })).status, 101);
  } finally {
    Date.now = real;
  }
});

test("a single route survives a day with one hundred complete socket losses", async () => {
  const h = harness();
  h.env.ROUTES = JSON.stringify(Array.from({ length: 1 }, (_, n) => String(n)));
  const real = Date.now;
  let clock = 10 * 86400000;
  Date.now = () => clock;
  try {
    h.restart();
    for (let n = 0; n < 100; n++) {
      clock += 60000;
      assert.equal(
        (await upgrade(h, { "X-Homepool-Bridge": "1" })).status,
        101,
      );
      const b = h.sockets.at(-1);
      await h.send(b, admission("bridge", "ab".repeat(32)));
      assert.equal(h.relay.bridge(), b);
      for (let token = 0; token < 8; token++) {
        clock += 75;
        await h.send(b, tokenSet(new Uint8Array(32).fill(token), 1, token));
        assert.equal(b.readyState, 1);
      }
      h.sockets.length = 0;
      h.restart();
    }
    assert.equal((await upgrade(h, { "X-Homepool-Bridge": "1" })).status, 101);
    assert.equal(h.values.get("bridge-admissions").daily, 101);
    clock += 86400000;
    assert.equal((await upgrade(h, { "X-Homepool-Bridge": "1" })).status, 101);
  } finally {
    Date.now = real;
  }
});
