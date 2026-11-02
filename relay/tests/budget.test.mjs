import { test } from "node:test";
import assert from "node:assert/strict";
import { harness } from "./harness.mjs";
import {
  admission,
  envelope,
  tokenSet,
  unwrap,
  encode,
} from "../../web/protocol/wire.mjs";
import { AckTracker } from "../ack.mjs";
import {
  ACK_INTERVAL,
  APPLICATION_WINDOW,
  RELAY_WINDOW,
  hash,
  BRIDGE_PING,
  BRIDGE_PONG,
} from "../limits.mjs";
import { EVENT_MS, batches } from "../budget.mjs";
const unhex = (s) => new Uint8Array(Buffer.from(s, "hex"));
async function setup(h) {
  const b = h.pending();
  await h.send(b, admission("bridge", "ab".repeat(32)));
  const token = "cd".repeat(32);
  await h.send(b, tokenSet(unhex(await hash(unhex(token))), 1, 42));
  const phones = [];
  for (let n = 0; n < 2; n++) {
    const p = h.pending();
    await h.send(p, admission("phone", token));
    phones.push(p);
  }
  b.sent = [];
  for (const p of phones) p.sent = [];
  return { b, phones };
}
test("24 h idle has no phone messages and zero keepalive wakes, writes, reads or alarms", async () => {
  const now = Date.now;
  let clock = 10 * 86400000;
  Date.now = () => clock;
  try {
    const h = harness(),
      { b, phones } = await setup(h),
      before = structuredClone(h.stats);
    for (let interval = 0; interval < 5760; interval++) {
      await h.send(b, BRIDGE_PING);
      assert.equal(b.sent.pop(), BRIDGE_PONG);
      if (interval < 5759) clock += 15000;
    }
    assert.deepEqual(h.stats, before);
    for (const p of phones)
      for (const f of [p.data.up, p.data.down]) assert.equal(f.sent, "0");
    h.restart();
    await h.send(b, BRIDGE_PING);
    assert.equal(b.sent.pop(), BRIDGE_PONG);
    assert.deepEqual(h.stats, before);
    assert.ok(ACK_INTERVAL + APPLICATION_WINDOW < RELAY_WINDOW);
  } finally {
    Date.now = now;
  }
});
test("lost sockets have zero budget undercount and only bounded conservative overcount", async () => {
  const h = harness();
  await setup(h);
  const before = structuredClone(h.relay.ledger.used),
    reserve = h.values.get("budget");
  for (const key of ["messages", "bytes", "failures", "writes", "durationMs"])
    assert.ok(reserve[key] >= before[key]);
  h.sockets.length = 0;
  h.restart();
  await h.relay.ledger.load();
  assert.deepEqual(h.relay.ledger.used, reserve);
  for (const key of ["messages", "bytes", "failures", "writes"])
    assert.ok(reserve[key] - before[key] < batches[key]);
  assert.ok(
    reserve.durationMs - before.durationMs < EVENT_MS + batches.durationMs,
  );
});
test("duration reservations stop work below the quota and recover next day", async () => {
  const h = harness({ dailyDurationMs: EVENT_MS + 1000 }),
    now = Date.now;
  let clock = 100000;
  Date.now = () => clock;
  try {
    let calls = 0;
    const work = () =>
      h.relay.run(async () => {
        calls++;
        clock += 1000;
      });
    await work();
    await work();
    assert.equal(calls, 2);
    assert.equal((await work()).status, 429);
    assert.equal(calls, 2);
    h.restart();
    assert.equal((await work()).status, 429);
    clock += 86400000;
    await work();
    assert.equal(calls, 3);
  } finally {
    Date.now = now;
  }
});

test("50 MB cost model bounds threshold/burst ACKs, including prior small messages", () => {
  const up = new AckTracker(1),
    down = new AckTracker(1);
  up.receive(65535); // Worst permitted carry from earlier idle messages.
  let remaining = 50000000,
    chunks = 0,
    credits = 0,
    acks = 0;
  while (remaining) {
    for (let n = 0; n < 8 && remaining; n++) {
      const bytes = Math.min(61440, remaining);
      remaining -= bytes;
      chunks++;
      if (up.receive(bytes + 64)) acks++;
    }
    if (up.flush()) acks++;
    credits++;
    if (down.receive(62)) acks++;
  }
  if (up.receive(512)) acks++;
  if (up.flush()) acks++;
  if (down.receive(512)) acks++;
  if (down.flush()) acks++;
  assert.equal(chunks, 814);
  assert.equal(credits, 102);
  assert.ok(acks <= 410, String(acks));
});

test("a failed reservation commit cannot forward the next data message", async () => {
  const h = harness(),
    { b, phones } = await setup(h),
    p = phones[0];
  while (h.relay.ledger.used.messages < h.relay.ledger.reserved.messages)
    await h.send(p, envelope("data", p.data.id, new Uint8Array(1)));
  const before = b.sent.filter((x) => unwrap(x).kind === "data").length;
  h.ctx.storage.put = async () => {
    throw Error("synthetic storage failure");
  };
  await h.send(p, envelope("data", p.data.id, new Uint8Array(1)));
  assert.equal(p.readyState, 3);
  assert.equal(b.sent.filter((x) => unwrap(x).kind === "data").length, before);
});
