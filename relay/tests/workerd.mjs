// Explicit local integration runner; Miniflare lives in scratch, never in this repo.
import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";
import assert from "node:assert/strict";
import { admission, envelope, unwrap } from "../../web/protocol/wire.mjs";
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
  const result = await transfer({
    async connect(role, token) {
      const response = await mf.dispatchFetch(
        new URL("/example", process.env.RELAY_TEST_ORIGIN),
        { headers: { Upgrade: "websocket" } },
      );
      assert.equal(response.status, 101);
      const ws = response.webSocket;
      ws.accept();
      sockets.push(ws);
      const q = [],
        waiters = [];
      ws.addEventListener("message", (e) => {
        const b = new Uint8Array(e.data);
        if (waiters.length) waiters.shift()(b);
        else q.push(b);
      });
      ws.next = () =>
        q.length
          ? Promise.resolve(q.shift())
          : new Promise((resolve, reject) => {
              const timer = setTimeout(
                () => reject(Error("receive deadline")),
                10000,
              );
              waiters.push((b) => {
                clearTimeout(timer);
                resolve(b);
              });
            });
      ws.send(admission(role, token));
      return ws;
    },
    send: async (s, b) => s.send(b),
    read: (s) => s.next(),
    pause: () => new Promise((r) => setTimeout(r, 150)),
  });
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
