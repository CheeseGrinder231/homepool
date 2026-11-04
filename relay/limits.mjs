// Fixed, non-authoritative liveness frames; never tokens or Noise messages.
export const BRIDGE_PING = "homepool/1/bridge-ping";
export const BRIDGE_PONG = "homepool/1/bridge-pong";
// Auto-responses bypass JavaScript and add no billed work.
export const AUTO_REQUESTS_PER_ROUTE = 0;
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
  dailyWrites: 8000,
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
// Fixed-size digests avoid a token-length dependent comparison loop.
export async function constantTimeToken(a, b) {
  const [x, y] = await Promise.all([hash(a), hash(b)]);
  let diff = 0;
  for (let i = 0; i < 64; i++) diff |= x.charCodeAt(i) ^ y.charCodeAt(i);
  return diff === 0;
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
  };
}

// Shared independently from phone work; 128 registrations per reconnect and
// eight reconnects per configured route fit these bounded control allowances.
export function bridgeLimits(env) {
  const count = Math.max(1, JSON.parse(env.ROUTES || "[]").length);
  return {
    ...defaults,
    dailyMessages: Math.floor(130000 / count),
    dailyWrites: Math.floor(30000 / count),
    dailyDurationMs: Math.floor(2000000 / count),
    dailyBytes: 1024 * 1024,
    dailyFailures: 8,
  };
}
