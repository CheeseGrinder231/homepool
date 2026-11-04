import { test } from "node:test";
import assert from "node:assert/strict";
import { harness } from "./harness.mjs";
import { admission, tokenSet, envelope } from "../../web/protocol/wire.mjs";
import { hash } from "../limits.mjs";

test("an owner day of isolated callbacks charges active time, excluding idle gaps", async () => {
  const real = Date.now;
  let clock = 10 * 86400000;
  Date.now = () => clock;
  try {
    const h = harness(),
      b = h.pending();
    await h.send(b, admission("bridge", "ab".repeat(32)));
    for (let n = 0; n < 1000; n++) {
      clock += 60000;
      await h.relay.run(async () => {
        clock += 2;
      });
      assert.equal(b.readyState, 1);
    }
    assert.equal(h.relay.ledger.used.durationMs, 2000);
  } finally {
    Date.now = real;
  }
});
test("spent phone duration and message allowances preserve bridge control and liveness", async () => {
  const h = harness(),
    b = h.pending();
  await h.send(b, admission("bridge", "ab".repeat(32)));
  h.relay.limits.dailyDurationMs = 0;
  await h.relay.run(async () => assert.fail("spent phone work ran"));
  assert.equal(b.readyState, 1);
  const token = new Uint8Array(32).fill(3);
  await h.send(
    b,
    tokenSet(new Uint8Array(Buffer.from(await hash(token), "hex")), 1, 42),
  );
  assert.equal(b.readyState, 1);
  assert.equal(Object.keys(h.values.get("tokens")).length, 1);
});
