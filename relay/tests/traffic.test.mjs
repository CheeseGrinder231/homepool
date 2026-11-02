import { test } from "node:test";
import assert from "node:assert/strict";
import { harness } from "./harness.mjs";
import {
  admission,
  envelope,
  tokenSet,
  unwrap,
} from "../../web/protocol/wire.mjs";
import { AckTracker } from "../ack.mjs";
import { hash } from "../limits.mjs";
const unhex = (s) => new Uint8Array(Buffer.from(s, "hex"));
test("2 GiB photo payload fits caps, with credit bursts, ACKs and operation-only pings", async () => {
  const now = Date.now;
  let clock = 10 * 86400000;
  Date.now = () => clock;
  try {
    const h = harness(),
      b = h.pending();
    await h.send(b, admission("bridge", "ab".repeat(32)));
    const token = "cd".repeat(32);
    await h.send(b, tokenSet(unhex(await hash(unhex(token))), 1, 42));
    const p = h.pending();
    await h.send(p, admission("phone", token));
    b.sent = [];
    p.sent = [];
    const up = new AckTracker(p.data.id),
      down = new AckTracker(p.data.id);
    let messages = 0,
      payload = 0,
      nextPing = clock + 15000;
    const move = async (from, to, frame, tracker) => {
      await h.send(from, frame);
      messages++;
      const received = to.sent.shift();
      assert.ok(received);
      assert.equal(received.length, frame.length);
      assert.equal(unwrap(received).session, p.data.id);
      assert.equal(unwrap(received).kind, "data");
      const a = tracker.receive(received.length);
      if (a) {
        await h.send(to, a);
        messages++;
      }
    };
    const flush = async (socket, tracker) => {
      const a = tracker.flush();
      if (a) {
        await h.send(socket, a);
        messages++;
      }
    };
    const full = envelope("data", p.data.id, new Uint8Array(61440 + 58)); // 64-byte total chunk overhead.
    const credit = envelope("data", p.data.id, new Uint8Array(56));
    for (let gib = 0; gib < 2; gib++) {
      const before = { ...h.stats },
        baseMessages = messages;
      let remaining = 1024 ** 3;
      while (remaining) {
        for (let n = 0; n < 8 && remaining; n++) {
          const bytes = Math.min(61440, remaining),
            frame =
              bytes === 61440
                ? full
                : envelope("data", p.data.id, new Uint8Array(bytes + 58));
          await move(p, b, frame, up);
          payload += bytes;
          remaining -= bytes;
        }
        await flush(b, up);
        await move(b, p, credit, down);
        await flush(p, down);
        if (clock >= nextPing) {
          const ping = envelope("data", p.data.id, new Uint8Array(52));
          await move(p, b, ping, up);
          await move(b, p, ping, down);
          nextPing = clock + 15000;
        }
        clock += 150;
        h.restart();
      }
      await flush(b, up);
      await flush(p, down);
      const result = {
        messages: messages - baseMessages,
        writes: h.stats.writes - before.writes,
        reads: h.stats.reads - before.reads,
      };
      assert.ok(result.messages < 31000, JSON.stringify(result));
      assert.ok(result.reads < 2300, JSON.stringify(result));
      assert.ok(result.writes < 500, JSON.stringify(result));
      console.log("per-GiB", JSON.stringify(result));
    }
    assert.equal(payload, 2 * 1024 ** 3);
    assert.equal(p.readyState, 1);
    assert.equal(b.readyState, 1);
    await h.relay.ledger.load();
    assert.ok(h.relay.ledger.used.bytes < 3 * 1024 ** 3);
  } finally {
    Date.now = now;
  }
});
