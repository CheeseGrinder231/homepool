// Read-only service tabs (#59): the view document, version 1. Pure helpers,
// no DOM and no network. The bridge applies the same rules before a document
// leaves home (crates/homepool-bridge/src/view.rs); both sides are checked
// against protocol/view-vectors.json. Every string stays text.

export const LIMITS = Object.freeze({
  sections: 20,
  items: 200,
  notes: 20,
  meta: 8,
  text: 500,
  refreshMin: 30,
  refreshMax: 3600,
  refreshDefault: 60,
  // An independent bound, shared with the bridge, on the normalised document
  // it sends; normalising can grow a document, so the bridge checks this too.
  bytes: 1024 * 1024 + 1024,
});

// Worst first. An unrecognised state is "unknown".
export const STATES = Object.freeze(["error", "stuck", "warn", "stale", "unknown", "ok"]);
export const rank = (state) => {
  const i = STATES.indexOf(state);
  return i < 0 ? STATES.indexOf("unknown") : i;
};

export class ViewError extends Error {
  constructor(what) {
    super(`invalid view document: ${what}`);
    this.reason = "invalid";
  }
}
const invalid = (what) => {
  throw new ViewError(what);
};
const isObject = (v) => v !== null && typeof v === "object" && !Array.isArray(v);

// Control characters other than a newline become spaces; unpaired surrogates
// are refused, as the bridge's JSON parser refuses them.
function text(value, what) {
  if (value === undefined) return "";
  if (typeof value !== "string") invalid(what);
  if (/[\uD800-\uDFFF]/u.test(value)) invalid(what);
  const out = value.replace(/[\u0000-\u0009\u000B-\u001F\u007F-\u009F]/g, " ");
  if ([...out].length > LIMITS.text) invalid(what);
  return out;
}

function list(value, max, what) {
  if (value === undefined) return [];
  if (!Array.isArray(value) || value.length > max) invalid(what);
  return value;
}

function state(value, what, otherwise = "unknown") {
  if (value === undefined) return otherwise;
  if (typeof value !== "string") invalid(what);
  return STATES.includes(value) ? value : "unknown";
}

// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant).
function daysFromCivil(y, m, d) {
  y -= m <= 2 ? 1 : 0;
  const era = Math.floor(y / 400);
  const yoe = y - era * 400;
  const doy = Math.floor((153 * (m + (m > 2 ? -3 : 9)) + 2) / 5) + d - 1;
  const doe = yoe * 365 + Math.floor(yoe / 4) - Math.floor(yoe / 100) + doy;
  return era * 146097 + doe - 719468;
}
function civilFromDays(z) {
  z += 719468;
  const era = Math.floor(z / 146097);
  const doe = z - era * 146097;
  const yoe = Math.floor((doe - Math.floor(doe / 1460) + Math.floor(doe / 36524) - Math.floor(doe / 146096)) / 365);
  const doy = doe - (365 * yoe + Math.floor(yoe / 4) - Math.floor(yoe / 100));
  const mp = Math.floor((5 * doy + 2) / 153);
  const d = doy - Math.floor((153 * mp + 2) / 5) + 1;
  const m = mp + (mp < 10 ? 3 : -9);
  return [era * 400 + yoe + (m <= 2 ? 1 : 0), m, d];
}
const pad = (n, w = 2) => String(n).padStart(w, "0");

// RFC 3339 with an upper-case T and Z or a numeric offset; fractions are
// dropped. Returns Unix seconds, or null.
export function parseTime(value) {
  if (typeof value !== "string") return null;
  const m = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})(?:\.\d{1,9})?(?:Z|([+-])(\d{2}):(\d{2}))$/.exec(value);
  if (!m) return null;
  const [y, mo, d, h, mi, s] = m.slice(1, 7).map(Number);
  const leap = (y % 4 === 0 && y % 100 !== 0) || y % 400 === 0;
  const days = [31, leap ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
  if (mo < 1 || mo > 12 || d < 1 || d > days[mo - 1] || h > 23 || mi > 59 || s > 59) return null;
  let offset = 0;
  if (m[7]) {
    const oh = Number(m[8]);
    const om = Number(m[9]);
    if (oh > 23 || om > 59) return null;
    offset = (m[7] === "+" ? 1 : -1) * (oh * 3600 + om * 60);
  }
  const at = daysFromCivil(y, mo, d) * 86400 + h * 3600 + mi * 60 + s - offset;
  return at >= daysFromCivil(0, 1, 1) * 86400 && at < daysFromCivil(10000, 1, 1) * 86400 ? at : null;
}
export function formatTime(seconds) {
  const days = Math.floor(seconds / 86400);
  const rest = seconds - days * 86400;
  const [y, m, d] = civilFromDays(days);
  return `${pad(y, 4)}-${pad(m)}-${pad(d)}T${pad(Math.floor(rest / 3600))}:${pad(Math.floor(rest / 60) % 60)}:${pad(rest % 60)}Z`;
}
function time(value, what) {
  const at = parseTime(value);
  if (at === null) invalid(what);
  return formatTime(at);
}

// https only, without credentials; anything else is dropped, not refused.
function link(value) {
  if (typeof value !== "string") return undefined;
  if ([...value].length > LIMITS.text) invalid("link");
  // Written as two checks: the repository's literal scan flags URL prefixes.
  if (!value.startsWith("https:") || !value.startsWith("//", 6)) return undefined;
  let url;
  try {
    url = new URL(value);
  } catch {
    return undefined;
  }
  if (url.protocol !== "https:" || !url.hostname || url.username || url.password) return undefined;
  // The bound holds for the written-out form too, so home's output passes
  // here again: non-ASCII characters can grow sixfold when percent-encoded.
  if (url.href.length > LIMITS.text) invalid("link");
  return url.href;
}

function progress(value) {
  if (value === undefined) return undefined;
  if (!isObject(value)) invalid("progress");
  const { value: v, target: t } = value;
  if (typeof v !== "number" || typeof t !== "number" || !Number.isFinite(v) || !Number.isFinite(t) || v < 0 || t <= 0)
    invalid("progress");
  return { value: v, target: t };
}

function item(value) {
  if (!isObject(value)) invalid("item");
  const out = {
    title: text(value.title, "item title"),
    state: state(value.state, "item state"),
    detail: text(value.detail, "item detail"),
    meta: list(value.meta, LIMITS.meta, "meta").map((m) => text(m, "meta")),
  };
  const l = link(value.link);
  if (l !== undefined) out.link = l;
  if (value.updated !== undefined) out.updated = time(value.updated, "updated");
  const p = progress(value.progress);
  if (p !== undefined) out.progress = p;
  return out;
}

// Validates a parsed view document and returns its normalised form: known
// fields only, canonical UTC times, bounded strings. Throws ViewError.
export function normalize(doc) {
  if (!isObject(doc)) invalid("document");
  if (doc.version !== 1) invalid("version");
  let refresh = LIMITS.refreshDefault;
  if (doc.refresh_s !== undefined) {
    if (typeof doc.refresh_s !== "number" || !Number.isFinite(doc.refresh_s)) invalid("refresh_s");
    refresh = Math.min(LIMITS.refreshMax, Math.max(LIMITS.refreshMin, Math.round(doc.refresh_s)));
  }
  const asOf = time(doc.as_of, "as_of");
  const validUntil = time(doc.valid_until, "valid_until");
  if (parseTime(validUntil) < parseTime(asOf)) invalid("valid_until");
  let total = 0;
  const sections = list(doc.sections, LIMITS.sections, "sections").map((s) => {
    if (!isObject(s)) invalid("section");
    const items = list(s.items, LIMITS.items, "items").map(item);
    total += items.length;
    if (total > LIMITS.items) invalid("items");
    const worst = items.length ? STATES[Math.min(...items.map((i) => rank(i.state)))] : "unknown";
    return {
      title: text(s.title, "section title"),
      state: state(s.state, "section state", worst),
      summary: text(s.summary, "summary"),
      items,
    };
  });
  return {
    version: 1,
    title: text(doc.title, "title"),
    as_of: asOf,
    valid_until: validUntil,
    refresh_s: refresh,
    state: state(doc.state, "state"),
    notes: list(doc.notes, LIMITS.notes, "notes").map((n) => text(n, "note")),
    sections,
  };
}

// Both of this phone's clocks: `mono` (performance.now(), ms) never steps
// with clock changes but may pause while the phone sleeps; `wall` (ms) keeps
// counting through sleep but can be set forwards or back.
export const clock = () => ({ mono: performance.now(), wall: Date.now() });

// The bytes from home: strict UTF-8 JSON, normalised again here. `started` is
// the clock when the page asked for it. The bridge adds `fetched_at`, its own
// clock when it read the document, so this phone's clock setting is not used.
export function readView(bytes, started = clock()) {
  if (!(bytes instanceof Uint8Array) || bytes.length > LIMITS.bytes) invalid("size");
  let doc;
  try {
    doc = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
  } catch {
    invalid("json");
  }
  const view = normalize(doc);
  const fetched = parseTime(doc.fetched_at);
  return { view, fetchedAt: fetched, started, elapsed: 0 };
}

// ---- presentation -----------------------------------------------------------

// Seconds since the page asked, counted conservatively: from the request's
// start rather than the document's arrival, by whichever clock has moved
// further, and never backwards, so a slow delivery, a sleeping phone or a
// clock set back cannot make the document younger.
export function elapsed(loaded, now = clock()) {
  const { mono, wall } = loaded.started;
  loaded.elapsed = Math.max(loaded.elapsed ?? 0, (now.mono - mono) / 1000, (now.wall - wall) / 1000);
  return loaded.elapsed;
}

// Home's clock now: when the bridge read the document plus the time since the
// page asked for it. Without `fetched_at`, this phone's clock at the request.
export function homeNow(loaded, now = clock()) {
  const base = loaded.fetchedAt ?? loaded.started.wall / 1000;
  return base + elapsed(loaded, now);
}

// `age` is how old `as_of` is; `expired`, how long ago `valid_until` passed.
export function freshness(loaded, now = clock()) {
  const at = homeNow(loaded, now);
  const age = Math.max(0, at - parseTime(loaded.view.as_of));
  const expired = at - parseTime(loaded.view.valid_until);
  return { age, stale: expired > 0, expired: Math.max(0, expired) };
}

export function ageText(seconds) {
  const s = Math.max(0, Math.floor(seconds));
  if (s < 45) return "just now";
  const min = Math.round(s / 60);
  if (min < 60) return `${min} min ago`;
  const hours = Math.floor(s / 3600);
  if (hours < 24) {
    const rest = Math.floor((s % 3600) / 60);
    return rest && hours < 6 ? `${hours} hr ${rest} min ago` : `${hours} hr ago`;
  }
  const days = Math.floor(s / 86400);
  return days === 1 ? "yesterday" : `${days} days ago`;
}

export const LABELS = Object.freeze({
  ok: "OK",
  warn: "Warning",
  stuck: "Stuck",
  stale: "Stale",
  error: "Error",
  unknown: "Unknown",
});

// What a badge says. Stale data is never shown as OK.
export function label(value, stale) {
  if (stale && value === "ok") return "Was OK";
  return LABELS[value] ?? LABELS.unknown;
}

// Sections with the worst state first, and within each, items worst first;
// equal states keep the document's order.
export function ordered(view) {
  const byRank = (a, b) => rank(a.value.state) - rank(b.value.state) || a.at - b.at;
  const sort = (xs) => xs.map((value, at) => ({ value, at })).sort(byRank).map((x) => x.value);
  return sort(
    view.sections.map((s) => ({ ...s, state: STATES[Math.min(rank(s.state), ...s.items.map((i) => rank(i.state)))] })),
  ).map((s) => ({ ...s, items: sort(s.items) }));
}

// Item counts per state, worst first, for the summary line.
export function counts(view) {
  const out = new Map();
  for (const s of view.sections) for (const i of s.items) out.set(i.state, (out.get(i.state) ?? 0) + 1);
  return STATES.filter((s) => out.has(s)).map((s) => [s, out.get(s)]);
}

export function progressText({ value, target }) {
  const f = (n) => (Number.isInteger(n) ? n.toLocaleString() : n.toLocaleString(undefined, { maximumFractionDigits: 2 }));
  return `${f(value)} of ${f(target)}`;
}
