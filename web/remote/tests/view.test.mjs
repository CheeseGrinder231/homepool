// The view document rules on the phone: the vectors the bridge also passes,
// hostile strings kept as text, links, staleness and what comes first.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";
import { LIMITS, ageText, counts, doubt, freshness, label, normalize, ordered, parseTime, readView } from "../view.mjs";

// "<https>" stands for the scheme prefix in the shared vectors.
const sub = (v) =>
  JSON.parse(
    JSON.stringify(v)
      .replaceAll("<https>", "https:" + "//")
      .replaceAll("<http>", "http:" + "//")
      .replaceAll("<HTTPS>", "HTTPS:" + "//"),
  );
const vectors = JSON.parse(await readFile(new URL("../../../protocol/view-vectors.json", import.meta.url), "utf8"));
const bytes = (v) => new Uint8Array(Buffer.from(typeof v === "string" ? v : JSON.stringify(v)));
const base = { version: 1, as_of: "2026-01-01T00:00:00Z", valid_until: "2026-01-01T00:01:00Z" };
const at = (s) => parseTime(s) * 1000;

test("shared vectors: same result as the bridge", () => {
  assert.ok(vectors.accept.length > 10 && vectors.refuse.length > 10);
  for (const { name, input, output } of vectors.accept) {
    assert.deepEqual(normalize(sub(input)), sub(output), name);
    // Normalising twice changes nothing: home's output passes again here.
    assert.deepEqual(normalize(normalize(sub(input))), sub(output), name);
    assert.deepEqual(readView(bytes(sub(input))).view, sub(output), name);
  }
  for (const { name, input } of vectors.refuse) assert.throws(() => normalize(sub(input)), { reason: "invalid" }, name);
  for (const { name, raw } of vectors.refuseRaw) assert.throws(() => readView(bytes(raw)), { reason: "invalid" }, name);
});

test("hostile strings stay strings and links are https only", () => {
  const html = '<img src=x onerror="alert(1)"><script>alert(2)</script>';
  const v = normalize({
    ...base,
    title: html,
    notes: [html],
    sections: [
      {
        title: html,
        summary: html,
        items: [
          { title: html, detail: html, meta: [html], link: ["javascript", "alert(1)"].join(":") },
          { title: "x", link: ["vbscript", "x"].join(":") },
          { title: "y", link: "//example.org/" },
          { title: "z", link: ["https", "//example.org/ok"].join(":") },
        ],
      },
    ],
  });
  assert.equal(v.title, html);
  assert.equal(v.sections[0].items[0].detail, html);
  assert.deepEqual(
    v.sections[0].items.map((i) => i.link),
    [undefined, undefined, undefined, ["https", "//example.org/ok"].join(":")],
  );
  assert.throws(() => readView(new Uint8Array(LIMITS.bytes + 1)), { reason: "invalid" });
});

// A phone clock reading: `mono` in ms since an arbitrary start, `wall` in ms.
const phone = (monoS, wallIso, plusS = 0) => ({ mono: monoS * 1000, wall: at(wallIso) + plusS * 1000 });

test("stale past valid_until by home's clock, even when the phone's clock is wrong", () => {
  const started = phone(0, "2030-01-01T00:00:00Z");
  const loaded = readView(bytes({ ...base, state: "ok", fetched_at: "2026-01-01T00:00:30Z" }), started);
  // The phone's clock reads 2030, but only 20 s have passed since the page asked.
  assert.deepEqual(freshness(loaded, phone(20, "2030-01-01T00:00:20Z")), { age: 50, stale: false, expired: 0, uncertain: false });
  assert.deepEqual(freshness(loaded, phone(31, "2030-01-01T00:00:31Z")), { age: 61, stale: true, expired: 1, uncertain: false });
  // A phone clock running behind cannot make old data look fresh either.
  const behind = readView(bytes({ ...base, fetched_at: "2026-01-01T00:05:00Z" }), phone(0, "2025-06-01T00:00:00Z"));
  assert.equal(freshness(behind, phone(0, "2025-06-01T00:00:00Z")).stale, true);
  // Without home's clock, the phone's at the request is the only one there is.
  const plain = readView(bytes(base), phone(0, "2026-01-01T00:00:00Z"));
  assert.equal(freshness(plain, phone(59, "2026-01-01T00:00:59Z")).stale, false);
  assert.equal(freshness(plain, phone(61, "2026-01-01T00:01:01Z")).stale, true);
  assert.equal(label("ok", true), "Was OK");
  assert.equal(label("ok", false), "OK");
  assert.equal(label("stuck", true), "Stuck");
  assert.equal(label("purple", false), "Unknown");
});

// Review of #60: time spent delivering the document counts against it.
test("a slow delivery ages the document from when the page asked", () => {
  const doc = bytes({ ...base, fetched_at: "2026-01-01T00:00:30Z" });
  // Asked at 0 s, delivered 120 s later: 30 + 120 is past valid_until (60).
  const loaded = readView(doc, phone(0, "2030-01-01T00:00:00Z"));
  assert.deepEqual(freshness(loaded, phone(120, "2030-01-01T00:02:00Z")), { age: 150, stale: true, expired: 90, uncertain: false });
  // Asked and delivered at once: fresh for the 30 s left.
  const quick = readView(doc, phone(0, "2030-01-01T00:00:00Z"));
  assert.equal(freshness(quick, phone(29, "2030-01-01T00:00:29Z")).stale, false);
  assert.equal(freshness(quick, phone(31, "2030-01-01T00:00:31Z")).stale, true);
});

test("changing the phone's clock after receipt never makes a document younger or current", () => {
  const doc = bytes({ ...base, fetched_at: "2026-01-01T00:00:30Z" });
  // Set back an hour after receipt: the steady clock still counts, and the
  // document is no longer shown as current.
  const back = readView(doc, phone(0, "2030-01-01T00:00:00Z"));
  assert.deepEqual(freshness(back, phone(20, "2029-12-31T23:00:20Z")), { age: 50, stale: false, expired: 0, uncertain: true });
  assert.equal(freshness(back, phone(31, "2029-12-31T23:00:31Z")).stale, true);
  // Moved 40 s ahead, the document goes stale; correcting the clock back
  // does not make it fresh again.
  const ahead = readView(doc, phone(0, "2030-01-01T00:00:00Z"));
  assert.equal(freshness(ahead, phone(5, "2030-01-01T00:00:45Z")).stale, true);
  assert.equal(freshness(ahead, phone(6, "2030-01-01T00:00:06Z")).stale, true);
  assert.equal(freshness(ahead, phone(7, "2030-01-01T00:00:07Z")).age, 77);
  // A sleeping phone whose steady clock paused: the wall clock still counts.
  const slept = readView(doc, phone(0, "2030-01-01T00:00:00Z"));
  assert.equal(freshness(slept, phone(2, "2030-01-01T00:10:00Z")).stale, true);
  // Small differences between the clocks are normal and change nothing.
  const steady = readView(doc, phone(0, "2030-01-01T00:00:00Z"));
  assert.equal(freshness(steady, phone(10, "2030-01-01T00:00:11.5Z")).uncertain, false);
});

// Delta review of #60: asleep with the steady clock paused while the wall
// clock is corrected back, neither clock alone shows the time that passed.
test("asleep while the clock is set back: in doubt until a new document, even after a failed refresh", () => {
  const W = at("2030-01-01T00:00:00Z");
  const loaded = readView(bytes({ ...base, state: "ok", fetched_at: "2026-01-01T00:00:30Z" }), { mono: 0, wall: W });
  assert.deepEqual(freshness(loaded, { mono: 1000, wall: W + 1000 }), { age: 31, stale: false, expired: 0, uncertain: false });
  // Ten minutes asleep, steady clock paused, wall clock set back an hour.
  const woke = { mono: 2000, wall: W + 600000 - 3600000 };
  const f = freshness(loaded, woke);
  assert.equal(f.stale, false, "neither clock can show the ten minutes");
  assert.equal(f.uncertain, true, "so it is never shown as current");
  assert.equal(label("ok", f.stale || f.uncertain), "Was OK");
  // The refresh on waking fails: the kept document is the same object and
  // stays in doubt however long the phone then stays awake.
  for (let s = 3; s < 60; s += 10)
    assert.equal(freshness(loaded, { mono: s * 1000, wall: woke.wall + (s - 2) * 1000 }).uncertain, true);
  // Only a new document is current again.
  const next = readView(bytes({ ...base, fetched_at: "2026-01-01T00:00:30Z" }), { mono: 70000, wall: woke.wall + 68000 });
  assert.equal(freshness(next, { mono: 71000, wall: woke.wall + 69000 }).uncertain, false);
});

test("a page hidden in the meantime puts the kept document in doubt", () => {
  const loaded = readView(bytes({ ...base, fetched_at: "2026-01-01T00:00:30Z" }), phone(0, "2030-01-01T00:00:00Z"));
  assert.equal(freshness(loaded, phone(5, "2030-01-01T00:00:05Z")).uncertain, false);
  doubt(loaded);
  // Even with clocks that agree, the phone may have slept unseen.
  assert.equal(freshness(loaded, phone(6, "2030-01-01T00:00:06Z")).uncertain, true);
});

test("after a clock jump and its correction, the age keeps moving", () => {
  const W = at("2030-01-01T00:00:00Z");
  const loaded = readView(bytes({ ...base, fetched_at: "2026-01-01T00:00:30Z" }), { mono: 0, wall: W });
  // An hour forward, then corrected: the count keeps its highest value and
  // then grows with the steady clock, rather than stopping at 3630.
  assert.equal(freshness(loaded, { mono: 1000, wall: W + 3600000 }).age, 3630);
  assert.equal(freshness(loaded, { mono: 2000, wall: W + 2000 }).age, 3631);
  assert.equal(freshness(loaded, { mono: 62000, wall: W + 62000 }).age, 3691);
  assert.equal(freshness(loaded, { mono: 62000, wall: W + 62000 }).uncertain, true);
});

test("what needs attention comes first; equal states keep their order", () => {
  const v = normalize({
    ...base,
    sections: [
      { title: "fine", items: [{ title: "a", state: "ok" }] },
      { title: "mixed", state: "ok", items: [{ title: "b", state: "ok" }, { title: "c", state: "warn" }, { title: "d", state: "error" }, { title: "e", state: "warn" }] },
      { title: "stuck", items: [{ title: "f", state: "stuck" }] },
      { title: "fine2", state: "ok" },
    ],
  });
  const o = ordered(v);
  assert.deepEqual(o.map((s) => s.title), ["mixed", "stuck", "fine", "fine2"]);
  assert.equal(o[0].state, "error", "a section shows its worst item");
  assert.deepEqual(o[0].items.map((i) => i.title), ["d", "c", "e", "b"]);
  assert.deepEqual(counts(v), [["error", 1], ["stuck", 1], ["warn", 2], ["ok", 2]]);
});

test("ages in plain words", () => {
  assert.equal(ageText(3), "just now");
  assert.equal(ageText(90), "2 min ago");
  assert.equal(ageText(3600 + 300), "1 hr 5 min ago");
  assert.equal(ageText(8 * 3600), "8 hr ago");
  assert.equal(ageText(26 * 3600), "yesterday");
  assert.equal(ageText(5 * 86400), "5 days ago");
});

test("already expired at publication stays stale regardless of either clock", () => {
  for (const fetched_at of [undefined, "2025-01-01T00:00:00Z", "2026-01-01T00:00:00Z", "2030-01-01T00:00:00Z"]) {
    const started = phone(0, "2024-01-01T00:00:00Z");
    const loaded = readView(bytes({ ...base, valid_until: "2025-12-31T23:57:00Z", fetched_at }), started);
    const initial = freshness(loaded, started);
    assert.equal(initial.stale, true);
    assert.ok(initial.expired >= 180, "expiry counts from valid_until, at least since publication");
    const later = freshness(loaded, phone(60, "2024-01-01T00:01:00Z"));
    assert.equal(later.stale, true);
    assert.equal(later.expired, initial.expired + 60);
    doubt(loaded);
    assert.equal(freshness(loaded, phone(61, "2023-12-31T23:01:01Z")).stale, true);
  }
  // Equality keeps the existing strict past-deadline rule.
  const equal = readView(bytes({ ...base, valid_until: base.as_of, fetched_at: base.as_of }), phone(0, base.as_of));
  assert.equal(freshness(equal, phone(0, base.as_of)).stale, false);
  assert.equal(freshness(equal, phone(1, base.as_of, 1)).stale, true);
});
