// Reserve batches durably before admitting work. Attachments retain exact live
// usage across hibernation; losing all sockets forfeits unused reservations.
// Local workerd bound includes the completion clock checkpoint. See README.
export const EVENT_MS = 2000;
export const batches = Object.freeze({
  messages: 128,
  bytes: 8 * 1024 * 1024,
  failures: 4,
  writes: 32,
  durationMs: 4000,
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
  constructor(
    ctx,
    limits,
    key = "budget",
    attachment = "account",
    reservationBatches = batches,
  ) {
    this.batches = reservationBatches;
    this.key = key;
    this.attachment = attachment;
    this.ctx = ctx;
    this.limits = limits;
    this.used = null;
    this.reserved = null;
  }
  async load() {
    const day = this.eventDay ?? Math.floor(Date.now() / 86400000);
    if (this.used?.day === day) return;
    let saved = await this.ctx.storage.get(this.key);
    if (saved?.day !== day)
      saved = { day, seq: 0, ...Object.fromEntries(keys.map((k) => [k, 0])) };
    saved.durationMs ??= 0;
    saved.writes ??= saved.messages;
    saved.seq ??= 0;
    this.reserved = structuredClone(saved);
    this.used = structuredClone(saved);
    const live = this.ctx
      .getWebSockets()
      .map((s) => s.deserializeAttachment()?.[this.attachment])
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
      if (a) s.serializeAttachment({ ...a, [this.attachment]: this.used });
    }
  }
  async charge(delta = {}) {
    await this.load();
    const next = structuredClone(this.used);
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
            next[k] + this.batches[k] - 1,
          );
      reserve.seq = next.seq;
      await this.ctx.storage.put(this.key, reserve);
      this.reserved = reserve;
    }
    this.used = next;
    this.snapshot();
    return true;
  }
  finish(started, ended) {
    // Called before returning to hibernation eligibility, never on the next
    // event. A real timer completion supplies an unfrozen I/O timestamp.
    this.used.durationMs -= EVENT_MS - Math.max(0, ended - started);
    this.used.seq++;
    this.snapshot();
  }
}
