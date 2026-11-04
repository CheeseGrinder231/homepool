// Explicit local integration runner; Miniflare lives in scratch, never in this repo.
import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";
import assert from "node:assert/strict";
import { admission, envelope, unwrap } from "../../web/protocol/wire.mjs";
import { BRIDGE_PING, BRIDGE_PONG } from "../limits.mjs";
import { transfer } from "./transfer.mjs";
const { Miniflare } = await import(pathToFileURL(process.env.MINIFLARE_MODULE));
const modules = {};
for (const file of [
  "relay/worker.mjs",
  "relay/route.mjs",
  "relay/limits.mjs",
  "relay/budget.mjs",
  "web/protocol/wire.mjs",
  "web/protocol/schema.mjs",
  "web/protocol/noise.mjs",
])
  modules[file] = {
    type: "esm",
    contents: await readFile(new URL("../../" + file, import.meta.url), "utf8"),
  };
const text = (value) => ({ type: "text", value });
process.chdir(process.env.TMPDIR);
const mf = new Miniflare({
  workers: [
    {
      config: {
        name: "relay",
        compatibilityDate: "2026-11-01",
        manifest: { mainModule: "relay/worker.mjs", modules },
        env: {
          KILL_SWITCH: text("false"),
          ROUTES: text('["example"]'),
          BRIDGE_TOKENS: text(JSON.stringify({ example: "ab".repeat(32) })),
          ROUTES_DO: {
            type: "durable-object",
            worker: "relay",
            exportName: "Relay",
          },
          UPGRADES: {
            type: "durable-object",
            worker: "relay",
            exportName: "UpgradeGate",
          },
        },
        exports: {
          Relay: { type: "durable-object", storage: "sqlite" },
          UpgradeGate: { type: "durable-object", storage: "sqlite" },
        },
      },
    },
  ],
});
const sockets = [];
try {
  assert.equal(
    (
      await mf.dispatchFetch(
        new URL("/unknown", process.env.RELAY_TEST_ORIGIN),
        { headers: { Upgrade: "websocket" } },
      )
    ).status,
    404,
  );
  // Headerless legacy admission and bad headers share the same refusal path.
  // None of these synthetic failures may claim a bridge-reserved grant.
  for (const authorization of [
    undefined,
    "Bearer " + "00".repeat(32),
    "Bearer garbled",
  ]) {
    const response = await mf.dispatchFetch(
      new URL("/example", process.env.RELAY_TEST_ORIGIN),
      {
        headers: {
          Upgrade: "websocket",
          ...(authorization ? { Authorization: authorization } : {}),
        },
      },
    );
    assert.equal(response.status, 101);
    const ws = response.webSocket;
    sockets.push(ws);
    ws.accept();
    const closed = new Promise((resolve, reject) => {
      const deadline = setTimeout(
        () => reject(Error("refusal deadline")),
        10000,
      );
      ws.addEventListener(
        "close",
        (event) => {
          clearTimeout(deadline);
          resolve(event.code);
        },
        { once: true },
      );
    });
    ws.send(admission("bridge", "00".repeat(32)));
    assert.equal(await closed, 1000);
  }
  sockets.length = 0;
  const result = await transfer({
    async connect(role, token) {
      for (let attempt = 0; attempt < 5; attempt++) {
        const response = await mf.dispatchFetch(
          new URL("/example", process.env.RELAY_TEST_ORIGIN),
          { headers: { Upgrade: "websocket" } },
        );
        assert.equal(response.status, 101);
        const ws = response.webSocket;
        ws.accept();
        const q = [],
          waiters = [];
        let closed = false;
        ws.addEventListener("message", (e) => {
          const value =
            typeof e.data === "string" ? e.data : new Uint8Array(e.data);
          if (waiters.length) waiters.shift().resolve(value);
          else q.push(value);
        });
        ws.addEventListener("close", () => {
          closed = true;
          for (const w of waiters.splice(0)) w.reject(Error("closed"));
        });
        ws.next = () =>
          q.length
            ? Promise.resolve(q.shift())
            : closed
              ? Promise.reject(Error("closed"))
              : new Promise((resolve, reject) => {
                  const timer = setTimeout(
                    () => reject(Error("receive deadline")),
                    10000,
                  );
                  waiters.push({
                    resolve: (b) => {
                      clearTimeout(timer);
                      resolve(b);
                    },
                    reject: (e) => {
                      clearTimeout(timer);
                      reject(e);
                    },
                  });
                });
        ws.send(admission(role, token));
        if (role === "phone") {
          try {
            const opened = await ws.next();
            assert.equal(unwrap(opened).kind, "open");
            q.unshift(opened);
          } catch (error) {
            ws.close();
            if (attempt === 4) throw error;
            await new Promise((r) => setTimeout(r, 100));
            continue;
          }
        }
        sockets.push(ws);
        return ws;
      }
    },
    send: async (s, b) => s.send(b),
    read: (s) => s.next(),
    pause: () => new Promise((r) => setTimeout(r, 150)),
  });
  await mf.unsafeEvictDurableObject("relay", "Relay", {
    name: "example",
    webSockets: "hibernate",
  });
  for (let n = 0; n < 100; n++) {
    sockets[0].send(BRIDGE_PING);
    assert.equal(await sockets[0].next(), BRIDGE_PONG);
  }
  // A matching text frame would close the socket if delivered to the JS handler.
  // Deliberately withhold phone ACKs; the sixteenth max-size envelope must fail.
  const [bridge, slow] = sockets;
  for (let n = 0; n < 16; n++)
    bridge.send(envelope("data", result.sessions[0], new Uint8Array(65535)));
  let bytes = 0;
  for (;;) {
    const e = unwrap(await slow.next());
    if (e.kind === "error") break;
    assert.equal(e.kind, "data");
    bytes += e.data.length + 6;
  }
  assert.ok(bytes <= 1024 * 1024);
  assert.equal(bytes, 15 * 65541);
  assert.equal(unwrap(await bridge.next()).kind, "error");
  console.log(
    JSON.stringify({
      runtime: "workerd",
      ...result,
      slowReceiverForwarded: bytes,
    }),
  );
} finally {
  for (const s of sockets)
    try {
      s.close();
    } catch {}
  await mf.dispose();
}
