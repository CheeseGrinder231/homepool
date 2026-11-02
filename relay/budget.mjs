// Reserve batches durably before admitting work. Attachments retain exact live
// usage across hibernation; losing all sockets forfeits unused reservations.
export const EVENT_MS = 31000; // blockConcurrencyWhile times out after 30 s.
export const batches = Object.freeze({
  messages: 128,
  bytes: 8 * 1024 * 1024,
  failures: 4,
  writes: 32,
  durationMs: 124000,
});
const keys = Object.keys(batches);
const capKey = {
  messages: "dailyMessages",
  bytes: "dailyBytes",
  failures: "dailyFailures",
  writes: "dailyWrites",
  durationMs: "dailyDurationMs",
};
export class Budget {
  constructor(ctx, limits) {
    this.ctx = ctx;
    this.limits = limits;
    this.used = null;
    this.reserved = null;
  }
  async load() {
    const day = this.eventDay ?? Math.floor(Date.now() / 86400000);
    if (this.used?.day === day) return;
    let saved = await this.ctx.storage.get("budget");
    if (saved?.day !== day)
      saved = { day, seq: 0, ...Object.fromEntries(keys.map((k) => [k, 0])) };
    saved.durationMs ??= 0;
    saved.writes ??= saved.messages;
    saved.seq ??= 0;
    this.reserved = structuredClone(saved);
    this.used = structuredClone(saved);
    const live = this.ctx
      .getWebSockets()
      .map((s) => s.deserializeAttachment()?.account)
      .filter(
        (a) =>
          a?.day === day &&
          keys.every(
            (k) => Number.isSafeInteger(a[k]) && a[k] >= 0 && a[k] <= saved[k],
          ),
      );
    if (live.length)
      this.used = structuredClone(
        live.reduce((a, b) => (a.seq > b.seq ? a : b)),
      );
  }
  snapshot() {
    for (const s of this.ctx.getWebSockets()) {
      const a = s.deserializeAttachment();
      if (a) s.serializeAttachment({ ...a, account: this.used });
    }
  }
  async charge(delta = {}) {
    await this.load();
    const next = structuredClone(this.used);
    if (delta.pendingSince !== undefined)
      next.pendingSince = delta.pendingSince;
    next.seq++;
    for (const k of keys) next[k] += delta[k] ?? 0;
    const fits = () => keys.every((k) => next[k] <= this.limits[capKey[k]]);
    if (!fits()) return false;
    if (keys.some((k) => next[k] > this.reserved[k])) {
      next.writes++; // The reservation row itself is never a hidden write.
      if (!fits()) return false;
      const reserve = structuredClone(this.reserved);
      for (const k of keys)
        if (next[k] > reserve[k])
          reserve[k] = Math.min(
            this.limits[capKey[k]],
            next[k] + batches[k] - 1,
          );
      reserve.seq = next.seq;
      await this.ctx.storage.put("budget", reserve);
      this.reserved = reserve;
    }
    this.used = next;
    this.snapshot();
    return true;
  }
  finishPrevious(now) {
    if (this.used.pendingSince === undefined) return;
    // The next event's I/O timestamp bounds the previous callback's duration.
    // Charge the entire gap up to 31 seconds, including idle time: overcount is
    // deliberate. Unlike an in-callback timer, this does not assume a live clock.
    const elapsed = Math.max(0, now - this.used.pendingSince);
    this.used.durationMs -= EVENT_MS - Math.min(EVENT_MS, elapsed);
    delete this.used.pendingSince;
    this.used.seq++;
  }
}
