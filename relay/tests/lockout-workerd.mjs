import { readFile, writeFile, mkdtemp } from "node:fs/promises";
import { spawn, spawnSync } from "node:child_process";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
const root = process.cwd(),
  scratch = await mkdtemp(join(process.env.TMPDIR, "lockout-"));
// Explicit runner: synthetic scratch state only; requires a locally built bridge.
import assert from "node:assert/strict";
const { Miniflare } = await import(pathToFileURL(process.env.MINIFLARE_MODULE));
const binary = process.env.BRIDGE_TEST_BINARY;
const mode = process.argv[2] || "lockout";
const messages = [];
const files = [
  "relay/worker.mjs",
  "relay/route.mjs",
  "relay/limits.mjs",
  "relay/budget.mjs",
  "web/protocol/wire.mjs",
  "web/protocol/schema.mjs",
  "web/protocol/noise.mjs",
];
const modules = {};
for (const file of files) {
  let contents = await readFile(join(root, file), "utf8");
  if (mode === "hostile" && file === "relay/limits.mjs")
    contents = contents
      .replace("dailyUpgrades: 1000", "dailyUpgrades: 5")
      .replace("dailyFailures: 100", "dailyFailures: 2");
  if (mode === "guard" && file === "relay/limits.mjs")
    contents = contents.replace("dailyMessages: 570000", "dailyMessages: 0");
  if (mode === "lockout" && file === "relay/limits.mjs")
    contents = contents.replace(
      "dailyDurationMs: 16000000",
      "dailyDurationMs: 32000",
    );
  if (mode === "spiral" && file === "relay/route.mjs")
    contents = contents.replace(
      'this.save(s, { role: "bridge", next: 1 });',
      'this.save(s, { role: "bridge", next: 1 }); console.log("fixture bridge admission", Date.now()); setTimeout(() => this.drop(s, true), 100);',
    );
  modules[file] = { type: "esm", contents };
}
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
let bridge;
try {
  const origin = (await mf.ready).origin;
  const state = join(scratch, "state");
  spawnSync(binary, ["init", state]);
  const put = async (name, data) => {
    const p = join(scratch, name);
    await writeFile(p, typeof data === "string" ? data : JSON.stringify(data), {
      mode: 0o600,
    });
    return p;
  };
  const token = await put("token", "ab".repeat(32));
  const credentials = await put("credentials", {
    version: 1,
    bucket: "example",
    aws_access_key_id: "synthetic",
    aws_secret_access_key: "synthetic",
  });
  const config = await put("config", {
    state_dir: state,
    relay_url: origin.replace("http:", "ws:") + "/example",
    route: "example",
    bridge_token_file: token,
    page_origin: new URL("/", origin.replace(/^http:/, "https:")).href,
    s3_endpoint: origin,
    bucket: "example",
    region: "example",
    credentials_file: credentials,
  });
  bridge = spawn(binary, ["run", config], {
    stdio: ["ignore", "ignore", "pipe"],
  });
  const began = Date.now();
  bridge.stderr.on("data", (b) => {
    messages.push(b.toString());
    console.log(
      "bridge",
      ((Date.now() - began) / 1000).toFixed(2),
      b.toString().trim(),
    );
  });
  // Workerd access logs expose upgrade/refusal cadence without bridge secrets.
  if (mode === "lockout" || mode === "guard") {
    await new Promise((r) => setTimeout(r, mode === "guard" ? 1000 : 32000));
    const response = await mf.dispatchFetch(origin + "/example", {
      headers: { Upgrade: "websocket" },
    });
    console.log("phone after idle", response.status);
    assert.equal(response.status, mode === "guard" ? 429 : 101);
    response.webSocket?.accept();
    response.webSocket?.close();
    await new Promise((r) => setTimeout(r, 6500));
    assert.equal(messages.join("").split("connection up").length - 1, 1);
    assert.ok(!messages.join("").includes("connection down"));
    console.log("bridge preserved");
  } else if (mode === "hostile") {
    await new Promise((r) => setTimeout(r, 1000));
    for (let n = 0; n < 10; n++) {
      const response = await mf.dispatchFetch(origin + "/example", {
        headers: { Upgrade: "websocket" },
      });
      if (response.webSocket) {
        response.webSocket.accept();
        for (let frame = 0; frame < 100; frame++) {
          try {
            response.webSocket.send(new Uint8Array(34));
          } catch {
            break;
          }
        }
        await new Promise((r) => setTimeout(r, 30));
        response.webSocket.close();
      }
    }
    assert.equal(
      (
        await mf.dispatchFetch(origin + "/example", {
          headers: { Upgrade: "websocket" },
        })
      ).status,
      429,
    );
    assert.ok(!messages.join("").includes("connection down"));
    await mf.unsafeEvictDurableObject("relay", "Relay", {
      name: "example",
      webSockets: "close",
    });
    await new Promise((r) => setTimeout(r, 10000));
    const transcript = messages.join("");
    assert.ok(transcript.split("connection up").length - 1 >= 2);
    assert.ok(
      transcript.lastIndexOf("connection up") >
        transcript.lastIndexOf("connection down"),
    );
    console.log("bridge recovered through exhausted hostile ingress");
  } else {
    await new Promise((r) => setTimeout(r, 12000));
    const delays = [...messages.join("").matchAll(/back-off (\d+) ms/g)].map(
      (m) => Number(m[1]),
    );
    assert.equal(delays.length, 4);
    for (let n = 0; n < 4; n++)
      assert.ok(delays[n] >= 1000 * 2 ** n && delays[n] <= 1000 * 2 ** n + 255);
  }
} finally {
  bridge?.kill("SIGKILL");
  await mf.dispose();
}
