import { test } from "node:test";
import assert from "node:assert/strict";
import { harness } from "./harness.mjs";
import { admission } from "../../web/protocol/wire.mjs";
import { transfer } from "./transfer.mjs";
test("two JS Noise phones, one bridge, interleaved 10 MiB, half-window credits", async () => {
  const h = harness();
  const result = await transfer({
    connect: async (role, token) => {
      const s = h.pending();
      await h.send(s, admission(role, token));
      return s;
    },
    pause: () => new Promise((r) => setTimeout(r, 150)),
    send: (s, b) => h.send(s, b),
    read: async (s) => {
      assert.ok(s.sent.length, "expected output");
      return s.sent.shift();
    },
  });
  assert.equal(result.bytes, 10 * 1024 * 1024);
  assert.equal(h.relay.phones().length, 2);
  // Each endpoint cumulatively ACKed the final burst; no data remains outstanding.
  for (const p of h.relay.phones())
    for (const f of [p.data.up, p.data.down]) assert.equal(f.sent, f.acked);
});
