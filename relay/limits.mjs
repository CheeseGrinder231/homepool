// Fixed, non-authoritative liveness frames; never tokens or Noise messages.
export const BRIDGE_PING = "homepool/1/bridge-ping";
export const BRIDGE_PONG = "homepool/1/bridge-pong";
// Auto-response adds no duration; conservatively retain incoming 20:1 units.
export const AUTO_REQUESTS_PER_ROUTE = 5760 / 20;
export const BRIDGE_MESSAGE_UNITS = 130000;
// Gate and route upgrade calls, alarms and closes; see relay/README.md.
export const AUXILIARY_REQUESTS = 14000;
export const RELAY_WINDOW = 1024 * 1024;
export const ACK_INTERVAL = 64 * 1024;
// Includes envelope/schema/AEAD overhead, not just plaintext file bytes.
export const APPLICATION_WINDOW = RELAY_WINDOW / 2;
export const defaults = Object.freeze({
  sessions: 4,
  pending: 8,
  admissionMs: 5000,
  messagesPerSecond: 256,
  bytesPerSecond: 8 * 1024 * 1024,
  controlsPerSecond: 32,
  controlBytesPerSecond: 4096,
  dailyBytes: 3 * 1024 ** 3,
  dailyMessages: 570000,
  dailyWrites: 7000,
  dailyDurationMs: 16000000,
  dailyUpgrades: 1000,
  bridgeUpgrades: 8,
  dailyFailures: 100,
  upgradesPerSecond: 20,
  tokens: 128,
  maxRoutes: 100,
});
export const hex = (b) =>
  Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
export const hash = async (b) =>
  hex(new Uint8Array(await crypto.subtle.digest("SHA-256", b)));
// Digests have a fixed length; compare every position without an early mismatch.
function sameHash(x, y) {
  let diff = 0;
  for (let i = 0; i < 64; i++) diff |= x.charCodeAt(i) ^ y.charCodeAt(i);
  return diff === 0;
}
export const tokenBytes = (s) =>
  typeof s === "string" && /^[0-9a-f]{64}$/.test(s)
    ? Uint8Array.from(s.match(/../g), (x) => parseInt(x, 16))
    : null;
// Bindings are immutable within a deployment. Retain only the derived hash in
// this cache; keep the existing private BRIDGE_TOKENS configuration format.
const bridgeHashes = new WeakMap();
export async function verifyBridgeToken(env, route, token) {
  let hashes = bridgeHashes.get(env);
  if (!hashes) bridgeHashes.set(env, (hashes = new Map()));
  if (!hashes.has(route)) {
    hashes.set(
      route,
      (async () => {
        let configured;
        try {
          configured = tokenBytes(JSON.parse(env.BRIDGE_TOKENS)[route]);
        } catch {
          return null;
        } // Never expose a private configuration parse error.
        return configured ? hash(configured) : null;
      })(),
    );
  }
  const expected = await hashes.get(route);
  const actual = await hash(token);
  return sameHash(actual, expected ?? "0".repeat(64)) && expected !== null;
}
export function rate(a, now, bytes, control, limits) {
  const second = Math.floor(now / 1000);
  if (a.second !== second)
    Object.assign(a, {
      second,
      messages: 0,
      bytes: 0,
      controls: 0,
      controlBytes: 0,
    });
  a.messages++;
  a.bytes += bytes;
  if (control) {
    a.controls++;
    a.controlBytes += bytes;
  }
  return (
    a.messages <= limits.messagesPerSecond &&
    a.bytes <= limits.bytesPerSecond &&
    a.controls <= limits.controlsPerSecond &&
    a.controlBytes <= limits.controlBytesPerSecond
  );
}

// Share admitted-work budgets across configured routes; they are not per-account
// quota reservations. Rejected requests and other Workers can still consume quota.
export function routeLimits(env) {
  const count = Math.max(1, JSON.parse(env.ROUTES || "[]").length);
  return {
    ...defaults,
    ...Object.fromEntries(
      [
        "dailyBytes",
        "dailyMessages",
        "dailyWrites",
        "dailyFailures",
        "dailyDurationMs",
      ].map((k) => [k, Math.floor(defaults[k] / count)]),
    ),
    dailyMessages: Math.floor(
      Math.min(
        defaults.dailyMessages,
        (50000 -
          AUXILIARY_REQUESTS -
          BRIDGE_MESSAGE_UNITS / 20 -
          count * AUTO_REQUESTS_PER_ROUTE) *
          20,
      ) / count,
    ),
  };
}

// Shared independently from phone work; 128 registrations per reconnect and
// eight reconnects per configured route fit these bounded control allowances.
export function bridgeLimits(env) {
  const count = Math.max(1, JSON.parse(env.ROUTES || "[]").length);
  return {
    ...defaults,
    dailyMessages: Math.floor(BRIDGE_MESSAGE_UNITS / count),
    dailyWrites: Math.floor(30000 / count),
    dailyDurationMs: Math.floor(2000000 / count),
    dailyBytes: 1024 * 1024,
    dailyFailures: 8,
  };
}
