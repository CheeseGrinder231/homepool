export const RELAY_WINDOW = 1024 * 1024;
export const ACK_INTERVAL = 256 * 1024;
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
  dailyBytes: 256 * 1024 ** 2,
  dailyMessages: 60000,
  dailyWrites: 75000,
  dailyUpgrades: 100,
  dailyFailures: 100,
  upgradesPerSecond: 20,
  tokens: 128,
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
      ["dailyBytes", "dailyMessages", "dailyWrites", "dailyFailures"].map(
        (k) => [k, Math.floor(defaults[k] / count)],
      ),
    ),
  };
}
