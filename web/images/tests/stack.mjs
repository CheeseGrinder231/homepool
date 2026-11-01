// Starts a throwaway Garage and a throwaway Caddy running the example site on
// loopback ports, for tests. Nothing here touches a real installation: data,
// Caddy's local CA and configuration all live under a fresh temporary directory.
import { spawn, spawnSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import { lookup } from "node:dns/promises";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { connect, createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
export const pageRoot = resolve(here, "..");
export const caddyfile = resolve(here, "..", "..", "..", "docs", "examples", "images.Caddyfile");

export const BUCKET = "homepool-consumer-1";
export const REGION = "garage";

// Tracked files carry no addresses or URLs (docs/structure.md), so the
// loopback address is resolved here and origins are assembled from parts.
const loopback = (await lookup("localhost", { family: 4 })).address;
const origin = (scheme, host) => `${scheme}://${host}`;

export function freePort() {
  return new Promise((done, fail) => {
    const server = createServer();
    server.once("error", fail);
    server.listen(0, loopback, () => {
      const { port } = server.address();
      server.close(() => done(port));
    });
  });
}

const sleep = (ms) => new Promise((done) => setTimeout(done, ms));

async function waitFor(check, what, attempts = 200) {
  for (let i = 0; i < attempts; i += 1) {
    if (await check()) return;
    await sleep(100);
  }
  throw new Error(`timed out waiting for ${what}`);
}

function run(command, args, { input } = {}) {
  const result = spawnSync(command, args, { input, encoding: "utf8" });
  if (result.status !== 0) throw new Error(`${command} ${args.join(" ")} failed: ${result.stderr}`);
  return result.stdout;
}

export async function startStack({ garage, caddy }) {
  const scratch = mkdtempSync(join(tmpdir(), "homepool-images-"));
  const [s3Port, rpcPort, adminPort, caddyPort] = await Promise.all([freePort(), freePort(), freePort(), freePort()]);
  const config = join(scratch, "garage.toml");
  writeFileSync(
    config,
    [
      `metadata_dir = "${join(scratch, "meta")}"`,
      `data_dir = "${join(scratch, "data")}"`,
      'db_engine = "sqlite"',
      "metadata_fsync = false",
      "data_fsync = false",
      "replication_factor = 1",
      `rpc_bind_addr = "${loopback}:${rpcPort}"`,
      `rpc_public_addr = "${loopback}:${rpcPort}"`,
      `rpc_secret = "${randomBytes(32).toString("hex")}"`,
      "[s3_api]",
      `s3_region = "${REGION}"`,
      `api_bind_addr = "${loopback}:${s3Port}"`,
      'root_domain = ".s3.garage.localhost"',
      "[admin]",
      `api_bind_addr = "${loopback}:${adminPort}"`,
      `admin_token = "${randomBytes(16).toString("hex")}"`,
      "",
    ].join("\n"),
  );
  const log = join(scratch, "garage.log");
  const garageProcess = spawn(garage, ["-c", config, "server"], { stdio: ["ignore", "ignore", "ignore"] });
  let garageFailure;
  garageProcess.on("error", (error) => {
    garageFailure = error;
  });
  const cli = (...args) => run(garage, ["-c", config, ...args]);
  await waitFor(() => {
    if (garageFailure) throw new Error(`garage did not start: ${garageFailure.message}`);
    if (garageProcess.exitCode !== null) throw new Error(`garage exited with ${garageProcess.exitCode}`);
    try {
      cli("status");
      return true;
    } catch {
      return false;
    }
  }, "garage");
  const nodeId = cli("node", "id", "-q").trim().split("@")[0];
  cli("layout", "assign", "-z", "zone-a", "-c", "1G", nodeId);
  cli("layout", "apply", "--version", "1");
  cli("bucket", "create", BUCKET);
  const created = cli("key", "create", "image-page-test");
  const accessKeyId = created.match(/Key ID:\s*(\S+)/)[1];
  const secretAccessKey = created.match(/Secret key:\s*(\S+)/)[1];
  cli("bucket", "allow", "--read", "--write", BUCKET, "--key", accessKeyId);
  await waitFor(async () => {
    const response = await fetch(`${origin("http", `${loopback}:${s3Port}`)}/`).catch(() => undefined);
    return response !== undefined;
  }, "garage s3 api");

  const caddyHome = join(scratch, "caddy");
  mkdirSync(caddyHome, { recursive: true });
  const wrapper = join(caddyHome, "Caddyfile");
  writeFileSync(wrapper, `{\n\tadmin off\n\tauto_https disable_redirects\n\tskip_install_trust\n}\nimport ${caddyfile}\n`);
  const address = `localhost:${caddyPort}`;
  const caddyProcess = spawn(caddy, ["run", "--config", wrapper, "--adapter", "caddyfile"], {
    stdio: ["ignore", "ignore", "pipe"],
    env: {
      ...process.env,
      XDG_DATA_HOME: join(caddyHome, "data"),
      XDG_CONFIG_HOME: join(caddyHome, "config"),
      HOME: caddyHome,
      HOMEPOOL_IMAGES_ADDRESS: address,
      HOMEPOOL_IMAGES_ROOT: pageRoot,
      HOMEPOOL_IMAGES_BUCKET: BUCKET,
      HOMEPOOL_GARAGE_S3: `localhost:${s3Port}`,
    },
  });
  let caddyLog = "";
  caddyProcess.stderr.on("data", (chunk) => {
    caddyLog += chunk;
  });
  caddyProcess.on("error", (error) => {
    caddyLog += `caddy did not start: ${error.message}`;
  });
  const rootCertificate = join(caddyHome, "data", "caddy", "pki", "authorities", "local", "root.crt");
  await waitFor(async () => {
    if (caddyProcess.exitCode !== null || caddyLog.includes("caddy did not start")) throw new Error(`caddy failed: ${caddyLog.slice(-800)}`);
    // The listener opens before the local certificate exists.
    if (!caddyLog.includes("certificate obtained successfully")) return false;
    try {
      readFileSync(rootCertificate);
    } catch {
      return false;
    }
    return new Promise((done) => {
      const socket = connect({ port: caddyPort, host: loopback }, () => {
        socket.destroy();
        done(true);
      });
      socket.on("error", () => done(false));
    });
  }, "caddy");

  return {
    scratch,
    host: address,
    origin: origin("https", address),
    garageOrigin: origin("http", `${loopback}:${s3Port}`),
    garageHost: `${loopback}:${s3Port}`,
    ca: readFileSync(rootCertificate),
    credentials: { accessKeyId, secretAccessKey, bucket: BUCKET, region: REGION, service: "s3" },
    log: () => caddyLog + readFileSync(log, { encoding: "utf8", flag: "a+" }),
    async stop() {
      caddyProcess.kill("SIGTERM");
      garageProcess.kill("SIGTERM");
      await Promise.all([
        new Promise((done) => caddyProcess.once("exit", done)),
        new Promise((done) => garageProcess.once("exit", done)),
      ]);
      rmSync(scratch, { recursive: true, force: true });
    },
  };
}
