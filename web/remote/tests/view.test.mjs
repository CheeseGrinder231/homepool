// The view document rules on the phone: the vectors the bridge also passes,
// hostile strings kept as text, links, staleness and what comes first.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";
import { LIMITS, ageText, counts, freshness, label, normalize, ordered, parseTime, readView } from "../view.mjs";

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

test("stale past valid_until by home's clock, even when the phone's clock is wrong", () => {
  const loaded = readView(bytes({ ...base, state: "ok", fetched_at: "2026-01-01T00:00:30Z" }), at("2030-01-01T00:00:00Z"));
  // The phone's clock reads 2030, but only 20 s have passed since home read it.
  const now = at("2030-01-01T00:00:20Z");
  assert.deepEqual(freshness(loaded, now), { age: 50, stale: false });
  assert.deepEqual(freshness(loaded, now + 11000), { age: 61, stale: true });
  // A phone clock running behind cannot make old data look fresh either.
  const behind = readView(bytes({ ...base, fetched_at: "2026-01-01T00:05:00Z" }), at("2025-06-01T00:00:00Z"));
  assert.equal(freshness(behind, at("2025-06-01T00:00:00Z")).stale, true);
  // Without home's clock, the phone's is the only one there is.
  const plain = readView(bytes(base), 0);
  assert.equal(freshness(plain, at("2026-01-01T00:00:59Z")).stale, false);
  assert.equal(freshness(plain, at("2026-01-01T00:01:01Z")).stale, true);
  assert.equal(label("ok", true), "Was OK");
  assert.equal(label("ok", false), "OK");
  assert.equal(label("stuck", true), "Stuck");
  assert.equal(label("purple", false), "Unknown");
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
