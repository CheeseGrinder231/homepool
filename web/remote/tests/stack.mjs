// Throwaway loopback stack for the remote page: Garage, the relay Worker in
// local workerd (serving the page from its static assets) and the bridge.
// Needs HOMEPOOL_TEST_GARAGE, MINIFLARE_MODULE and CARGO_TARGET_DIR with a
// built homepool-bridge. Reads no live configuration; everything is in TMPDIR.
import { spawn, spawnSync, execFile } from "node:child_process";
import { randomBytes } from "node:crypto";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createServer as httpServer, request as httpRequest } from "node:http";
import { createServer } from "node:net";
import { lookup } from "node:dns/promises";
import { join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { promisify } from "node:util";
import assert from "node:assert/strict";

const execute = promisify(execFile);
const root = fileURLToPath(new URL("../../../", import.meta.url));
const loopback = (await lookup("localhost", { family: 4 })).address;
const url = (scheme, rest) => `${scheme}://${rest}`;
const delay = (ms) => new Promise((r) => setTimeout(r, ms));
const port = () =>
  new Promise((r) => {
    const s = createServer();
    s.listen(0, loopback, () => {
      const p = s.address().port;
      s.close(() => r(p));
    });
  });
const privateFile = (path, data) =>
  writeFile(path, typeof data === "string" ? data : JSON.stringify(data), { mode: 0o600 });
async function wait(fn, tries = 200) {
  for (let i = 0; i < tries; i++) {
    try {
      return await fn();
    } catch {}
    await delay(100);
  }
  throw Error("local startup timeout");
}
function run(command, args) {
  const r = spawnSync(command, args, { encoding: "utf8" });
  assert.equal(r.status, 0, r.stderr);
  return r.stdout;
}

const RELAY_MODULES = [
  "relay/worker.mjs",
  "relay/route.mjs",
  "relay/limits.mjs",
  "relay/budget.mjs",
  "web/protocol/wire.mjs",
  "web/protocol/schema.mjs",
  "web/protocol/noise.mjs",
];

export async function startStack({ log = () => {} } = {}) {
  const garage = process.env.HOMEPOOL_TEST_GARAGE;
  const binary = resolve(process.env.CARGO_TARGET_DIR, "debug/homepool-bridge");
  const scratch = await mkdtemp(join(process.env.TMPDIR, "remote-page-"));
  const children = new Set();
  const child = (command, args) => {
    const p = spawn(command, args, { stdio: ["ignore", "ignore", "pipe"] });
    children.add(p);
    p.stderr.on("data", (b) => {
      if (command === binary) log(`bridge: ${b}`);
    });
    p.once("exit", () => children.delete(p));
    return p;
  };
  process.on("exit", () => {
    for (const p of children) p.kill("SIGKILL");
  });

  // Garage, behind a pass-through proxy that can withhold a PUT response
  // after the store has confirmed it.
  const s3Port = await port();
  const rpcPort = await port();
  const proxyPort = await port();
  const hold = { put: false, confirmed: false };
  const proxy = httpServer((req, res) => {
    const upstream = httpRequest(
      { host: loopback, port: s3Port, path: req.url, method: req.method, headers: req.headers },
      (response) => {
        if (hold.put && req.method === "PUT") {
          hold.confirmed = true;
          response.resume();
          return;
        }
        res.writeHead(response.statusCode, response.headers);
        response.pipe(res);
      },
    );
    upstream.on("error", () => res.destroy());
    req.pipe(upstream);
  });
  await new Promise((r) => proxy.listen(proxyPort, loopback, r));
  const garageConfig = join(scratch, "garage.toml");
  await privateFile(
    garageConfig,
    `metadata_dir = "${join(scratch, "meta")}"
data_dir = "${join(scratch, "data")}"
db_engine = "sqlite"
replication_factor = 1
rpc_bind_addr = "${loopback}:${rpcPort}"
rpc_public_addr = "${loopback}:${rpcPort}"
rpc_secret = "${randomBytes(32).toString("hex")}"
[s3_api]
s3_region = "garage"
api_bind_addr = "${loopback}:${s3Port}"
root_domain = ".s3.garage.localhost"
`,
  );
  const cli = (...args) => run(garage, ["-c", garageConfig, ...args]);
  child(garage, ["-c", garageConfig, "server"]);
  await wait(() => cli("status"));
  const node = cli("node", "id", "-q").trim().split("@")[0];
  cli("layout", "assign", "-z", "zone-a", "-c", "1G", node);
  cli("layout", "apply", "--version", "1");
  const bucket = "homepool-consumer-1";
  cli("bucket", "create", bucket);
  const created = cli("key", "create", "remote-page-test");
  const access = created.match(/Key ID:\s*(\S+)/)[1];
  const secret = created.match(/Secret key:\s*(\S+)/)[1];
  cli("bucket", "allow", "--read", "--write", bucket, "--key", access);
  log("garage ready");

  // The relay Worker with the page as its static assets, persisted in scratch
  // so a restart keeps Durable Object storage as a deploy would.
  const { Miniflare } = await import(pathToFileURL(process.env.MINIFLARE_MODULE));
  const modules = {};
  for (const file of RELAY_MODULES)
    modules[file] = { type: "esm", contents: await readFile(join(root, file), "utf8") };
  const text = (value) => ({ type: "text", value });
  const bridgeToken = randomBytes(32).toString("hex");
  const relayPort = await port();
  const persist = join(scratch, "relay-state");
  await mkdir(persist);
  const cwd = process.cwd();
  process.chdir(process.env.TMPDIR);
  const options = {
    port: relayPort,
    resourcePersistencePath: persist,
    enableDevRegistry: false,
    workers: [
      {
        config: {
          name: "relay",
          compatibilityDate: "2026-11-01",
          manifest: { mainModule: "relay/worker.mjs", modules },
          env: {
            KILL_SWITCH: text("false"),
            ROUTES: text('["example"]'),
            BRIDGE_TOKENS: text(JSON.stringify({ example: bridgeToken })),
            ROUTES_DO: { type: "durable-object", worker: "relay", exportName: "Relay" },
            UPGRADES: { type: "durable-object", worker: "relay", exportName: "UpgradeGate" },
            ASSETS: { type: "assets" },
          },
          exports: {
            Relay: { type: "durable-object", storage: "sqlite" },
            UpgradeGate: { type: "durable-object", storage: "sqlite" },
          },
          // As in relay/wrangler.toml: the Worker runs first for every path.
          assets: {
            directory: join(root, "web"),
            hasUserWorker: true,
            runWorkerFirst: true,
            htmlHandling: "none",
            notFoundHandling: "none",
          },
        },
      },
    ],
  };
  let mf = new Miniflare(options);
  await mf.ready;
  process.chdir(cwd);
  const origin = url("http", `${loopback}:${relayPort}`);
  log(`relay ready at ${origin}`);

  const stateDir = join(scratch, "state");
  run(binary, ["init", stateDir]);
  const credentials = join(scratch, "credentials.json");
  const tokenFile = join(scratch, "token");
  const bridgeConfig = join(scratch, "bridge.json");
  await privateFile(credentials, { version: 1, bucket, aws_access_key_id: access, aws_secret_access_key: secret });
  await privateFile(tokenFile, bridgeToken);
  await privateFile(bridgeConfig, {
    state_dir: stateDir,
    relay_url: url("ws", `${loopback}:${relayPort}/example`),
    route: "example",
    bridge_token_file: tokenFile,
    page_origin: url("https", "192.0.2.1/"),
    s3_endpoint: url("http", `${loopback}:${proxyPort}`),
    bucket,
    region: "garage",
    credentials_file: credentials,
  });
  const command = async (...args) =>
    (await execute(binary, [args[0], bridgeConfig, ...args.slice(1)])).stdout;
  let bridge = null;
  const stack = {
    origin,
    scratch,
    stateDir,
    hold,
    async startBridge() {
      bridge = child(binary, ["run", bridgeConfig]);
      await wait(() => command("devices"));
      // Connected and token registrations sent.
      await delay(1500);
    },
    async killBridge() {
      if (!bridge) return;
      const p = bridge;
      bridge = null;
      p.kill("SIGKILL");
      if (p.exitCode === null) await new Promise((r) => p.once("exit", r));
    },
    async stopRelay() {
      await mf.dispose();
      mf = null;
    },
    async startRelay() {
      process.chdir(process.env.TMPDIR);
      mf = new Miniflare(options);
      await mf.ready;
      process.chdir(cwd);
    },
    // The fragment the bridge would put in a QR code, on this test origin.
    async invite() {
      const link = JSON.parse(await command("invite")).link;
      return `${origin}/#${link.split("#")[1]}`;
    },
    async devices() {
      return JSON.parse(await command("devices")).devices;
    },
    async revoke(id) {
      return JSON.parse(await command("revoke", id));
    },
    async journal() {
      return JSON.parse(await readFile(join(stateDir, "state.json"), "utf8")).journal;
    },
    async editState(fn) {
      const path = join(stateDir, "state.json");
      const value = JSON.parse(await readFile(path, "utf8"));
      fn(value);
      await privateFile(path, JSON.stringify(value));
    },
    async stop() {
      await this.killBridge();
      if (mf) await mf.dispose();
      proxy.closeAllConnections();
      proxy.close();
      for (const p of children) p.kill("SIGKILL");
      await rm(scratch, { recursive: true, force: true });
    },
  };
  await stack.startBridge();
  log("bridge ready");
  return stack;
}
