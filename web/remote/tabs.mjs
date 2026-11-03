// Read-only service tabs (#59): one tab per service home lets this phone see,
// after Photos. Documents arrive over the same encrypted session as photos.
// Every string from home goes into textContent; links are https only.
//
// Fetching follows #57's visibility rules: when a tab opens, once when the
// page becomes visible again on that tab, and every refresh_s while visible.
// Nothing is fetched while the page is hidden, and nothing pings while idle.
import { PRIORITY, serviceGet, services } from "./client.mjs";
import { STATES, ageText, counts, freshness, homeNow, label, ordered, parseTime, progressText } from "./view.mjs";

const SERVICE_REASONS = {
  "service-unavailable": (t) => `${t} isn't answering at home right now. Check that it's running on the home computer, then try again.`,
  "service-too-large": (t) =>
    `${t} sent more than its size limit, so home didn't pass it on. Make its status smaller, or raise its limit in the bridge settings at home.`,
  "service-invalid": (t) =>
    `${t} sent something this page can't read. Check ${t} at home: its status may be in the wrong format.`,
  "service-not-allowed": (t) =>
    `This phone isn't allowed to see ${t}. To allow it, add this phone to ${t} in the bridge settings at home.`,
  "service-unknown": (t) => `${t} isn't set up at home anymore. Reload this page to update the list.`,
};
// A service problem: keep refreshing on schedule. Anything else is about the
// connection, which waits for the next action or return (#57).
const SERVICE_SIDE = new Set(["service-unavailable", "service-too-large", "service-invalid"]);
const TICK = 10000; // redraws ages and staleness; no network

const el = (tag, className, text) => {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
};
const badge = (state, stale) => el("span", `badge s-${STATES.includes(state) ? state : "unknown"}`, label(state, stale));
// A time in this phone's own words: "14:32", or with the day when not today.
const until = (iso) => {
  const d = new Date(iso);
  const time = d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  return d.toDateString() === new Date().toDateString() ? time : `${d.toLocaleDateString([], { month: "short", day: "numeric" })}, ${time}`;
};
const plural = (n, one, many) => `${n} ${n === 1 ? one : many}`;
const WORDS = {
  error: ["error", "errors"],
  stuck: ["stuck", "stuck"],
  warn: ["warning", "warnings"],
  stale: ["stale", "stale"],
  unknown: ["unknown", "unknown"],
  ok: ["OK", "OK"],
};

export function createServiceTabs({ remote, ui, store, say, isLibrary, onPhotos }) {
  const state = {
    list: [], // [{ id, title }]
    buttons: new Map(), // id -> tab button
    docs: new Map(), // id -> { loaded, error, loading }
    active: "photos",
    timer: null,
    ticker: null,
    generation: 0,
    listing: null,
    listed: false,
  };
  const title = (id) => state.list.find((s) => s.id === id)?.title ?? "This service";
  const message = (reason, id) => SERVICE_REASONS[reason]?.(title(id)) ?? say(reason);

  function stopTimers() {
    clearTimeout(state.timer);
    clearInterval(state.ticker);
    state.timer = null;
    state.ticker = null;
  }

  function showTabs() {
    const any = state.list.length > 0;
    ui.tabs.hidden = !any;
    ui.heading.textContent = any ? "Homepool" : "Homepool photos";
    const buttons = [ui.tabPhotos];
    const keep = new Map();
    for (const { id, title: name } of state.list) {
      const b = state.buttons.get(id) ?? el("button", "tab", name);
      b.type = "button";
      b.setAttribute("role", "tab");
      b.setAttribute("aria-controls", "service");
      b.textContent = name;
      b.onclick = () => select(id, { user: true });
      keep.set(id, b);
      buttons.push(b);
    }
    state.buttons = keep;
    ui.tabs.replaceChildren(...buttons);
    for (const [id, b] of [["photos", ui.tabPhotos], ...keep]) b.setAttribute("aria-selected", String(id === state.active));
  }

  // Shows a tab. Opening a service tab fetches it now, once.
  function select(id, { user = false, fetch = true } = {}) {
    if (id !== "photos" && !state.list.some((s) => s.id === id)) id = "photos";
    state.active = id;
    stopTimers();
    showTabs();
    ui.photos.hidden = id !== "photos";
    ui.service.hidden = id === "photos";
    if (user) store.put("tab", id === "photos" ? null : { id, title: title(id) }).catch(() => {});
    if (id === "photos") return onPhotos();
    ui.service.setAttribute("aria-label", title(id));
    render();
    if (fetch) load(id, user ? PRIORITY.open : PRIORITY.list);
    tick();
  }

  // ---- fetching ---------------------------------------------------------------

  function load(id, priority = PRIORITY.list) {
    if (document.hidden || !isLibrary()) return;
    const entry = state.docs.get(id) ?? { loaded: null, error: null, loading: null };
    state.docs.set(id, entry);
    if (entry.loading) {
      entry.loading.promote?.(priority);
      return;
    }
    clearTimeout(state.timer);
    const generation = state.generation;
    remote.queue.resume();
    const job = remote.run(priority, (link) => serviceGet(link, id));
    entry.loading = job;
    if (state.active === id) render();
    job.done
      .then((loaded) => {
        if (generation !== state.generation) return;
        entry.loaded = loaded;
        entry.error = null;
      })
      .catch((error) => {
        if (generation !== state.generation || error?.reason === "cancelled") return;
        entry.error = error?.reason ?? "unavailable";
      })
      .finally(() => {
        if (generation !== state.generation) return;
        entry.loading = null;
        mark(id);
        if (state.active !== id) return;
        render();
        // Removed or unrecognized phones go to the pairing screen elsewhere.
        if (!entry.error || SERVICE_SIDE.has(entry.error)) schedule(id);
      });
  }

  function schedule(id) {
    clearTimeout(state.timer);
    if (document.hidden || state.active !== id) return;
    const seconds = state.docs.get(id)?.loaded?.view.refresh_s ?? 60;
    state.timer = setTimeout(() => load(id), seconds * 1000);
  }

  function tick() {
    clearInterval(state.ticker);
    if (state.active === "photos" || document.hidden) return;
    state.ticker = setInterval(() => {
      render();
      mark(state.active);
    }, TICK);
  }

  // The tab shows a coloured mark, with words for screen readers, when its
  // service needs attention or is out of date.
  function mark(id) {
    const b = state.buttons.get(id);
    const loaded = state.docs.get(id)?.loaded;
    if (!b || !loaded) return;
    const { stale } = freshness(loaded);
    const worst = stale ? "stale" : worstOf(loaded.view);
    const attention = ["error", "stuck", "warn", "stale"].includes(worst);
    b.dataset.state = attention ? worst : "";
    b.setAttribute("aria-label", attention ? `${title(id)}, ${label(worst, false).toLowerCase()}` : title(id));
  }
  function worstOf(view) {
    const all = [view.state, ...view.sections.flatMap((s) => [s.state, ...s.items.map((i) => i.state)])];
    return STATES.find((s) => all.includes(s)) ?? "unknown";
  }

  // ---- drawing ---------------------------------------------------------------------

  function render() {
    const id = state.active;
    if (id === "photos") return;
    const entry = state.docs.get(id) ?? {};
    const name = title(id);
    const { loaded, error, loading } = entry;
    ui.service.setAttribute("aria-busy", String(Boolean(loading)));
    ui.serviceLoading.hidden = Boolean(loaded) || Boolean(error && !loading);
    ui.serviceLoadingText.textContent = `Asking home for ${name}…`;
    ui.serviceView.hidden = !loaded;
    ui.serviceProblem.hidden = !error;
    ui.serviceRetry.hidden = Boolean(loading);
    if (error) {
      const fresh = loaded && freshness(loaded);
      ui.serviceProblemText.textContent = loaded
        ? `${message(error, id)} Below is what it sent ${ageText(fresh.age)}.`
        : message(error, id);
    }
    if (loaded) draw(loaded, name, Boolean(loading));
  }

  function draw(loaded, name, checking) {
    const { view } = loaded;
    const { age, stale } = freshness(loaded);
    ui.service.classList.toggle("is-stale", stale);
    const overall = stale ? "stale" : view.state;
    ui.serviceState.className = `badge s-${overall}`;
    ui.serviceState.textContent = stale ? "Out of date" : label(view.state, false);
    ui.serviceTitle.textContent = view.title || name;
    ui.serviceAge.textContent = `Updated ${ageText(age)}${checking ? " · checking…" : ""}`;
    const tally = counts(view);
    const attention = tally.filter(([s]) => ["error", "stuck", "warn"].includes(s));
    const line = tally.map(([s, n]) => plural(n, ...WORDS[s])).join(" · ");
    ui.serviceCounts.textContent = stale ? `When last sent: ${line}` : line;
    ui.serviceCounts.hidden = tally.length === 0;
    ui.serviceStale.hidden = !stale;
    ui.serviceStale.textContent = stale
      ? `Out of date. ${name} said this was good until ${until(loaded.view.valid_until)} and hasn't sent anything newer, so it may have changed.`
      : "";
    ui.serviceNotes.replaceChildren(...view.notes.map((n) => el("li", "", n)));
    ui.serviceNotes.hidden = view.notes.length === 0;
    const sections = ordered(view);
    const now = homeNow(loaded);
    ui.serviceSections.replaceChildren(...sections.map((s) => section(s, stale, attention.length > 0, now)));
    ui.serviceNothing.hidden = sections.length > 0 || view.notes.length > 0;
  }

  // Sections that need attention start open; all-OK ones start folded.
  function section(s, stale, others, now) {
    const quiet = s.state === "ok" && s.items.every((i) => i.state === "ok");
    const box = el("details", `svc-section s-${s.state}`);
    box.open = !quiet || !others;
    const head = el("summary", "svc-section-head");
    head.append(badge(s.state, stale), el("span", "svc-section-title", s.title || "Untitled"));
    if (s.items.length) head.append(el("span", "svc-section-count", plural(s.items.length, "item", "items")));
    box.append(head);
    if (s.summary) box.append(el("p", "svc-summary", s.summary));
    if (s.items.length) {
      const list = el("ul", "svc-items");
      list.append(...s.items.map((i) => item(i, stale, now)));
      box.append(list);
    }
    return box;
  }

  function item(i, stale, now) {
    const li = el("li", `svc-item s-${i.state}`);
    const head = el("div", "svc-item-head");
    head.append(badge(i.state, stale), el("span", "svc-item-title", i.title || "Untitled"));
    li.append(head);
    if (i.detail) li.append(el("p", "svc-detail", i.detail));
    if (i.progress) {
      const row = el("div", "svc-progress");
      const bar = el("progress");
      bar.max = i.progress.target;
      bar.value = Math.min(i.progress.value, i.progress.target);
      row.append(bar, el("span", "", progressText(i.progress)));
      li.append(row);
    }
    if (i.meta.length) {
      const meta = el("p", "svc-meta");
      meta.append(...i.meta.map((m) => el("span", "", m)));
      li.append(meta);
    }
    if (i.updated || i.link) {
      const foot = el("p", "svc-foot");
      if (i.updated) foot.append(el("span", "", `Changed ${ageText(now - parseTime(i.updated))}`));
      if (i.link) {
        // Already checked: https only. Opens outside the page, with no opener
        // or referrer.
        const a = el("a", "svc-link", `Open ${new URL(i.link).hostname} ↗`);
        a.href = i.link;
        a.target = "_blank";
        a.rel = "noopener noreferrer";
        foot.append(a);
      }
      li.append(foot);
    }
    return li;
  }

  ui.serviceRetry.addEventListener("click", () => {
    if (state.active === "photos") return;
    remote.queue.resume();
    load(state.active, PRIORITY.open);
    render();
  });
  ui.tabPhotos.addEventListener("click", () => select("photos", { user: true }));

  return {
    get active() {
      return state.active;
    },
    // Asks home which services this phone may see, then restores the last
    // tab. Until the list arrives a remembered service tab shows as loading.
    async start() {
      const generation = state.generation;
      const saved = await store.get("tab").catch(() => null);
      if (saved?.id && !state.list.length) {
        state.list = [{ id: saved.id, title: saved.title }];
        select(saved.id);
      } else if (state.active === "photos") onPhotos();
      if (state.listing) return state.listing;
      state.listing = remote
        .run(PRIORITY.list, (link) => services(link))
        .done.then((list) => {
          if (generation !== state.generation) return;
          state.list = list;
          state.listed = true;
          showTabs();
          const keep = state.active !== "photos" && list.some((s) => s.id === state.active);
          if (!keep && state.active !== "photos") select("photos");
        })
        .catch(() => {
          // Shown by the connection status; tabs come with the next start.
          if (generation === state.generation && state.active !== "photos" && !state.docs.get(state.active)?.loaded)
            render();
        })
        .finally(() => {
          state.listing = null;
        });
      return state.listing;
    },
    refresh() {
      if (state.active !== "photos") load(state.active, PRIORITY.open);
    },
    // Page hidden: stop every timer; nothing is fetched until it returns.
    hidden() {
      stopTimers();
    },
    // Page visible again on a service tab: one fetch, then the schedule.
    visible() {
      if (!isLibrary()) return;
      if (!state.listed) this.start();
      if (state.active === "photos") return;
      render();
      load(state.active);
      tick();
    },
    reset() {
      state.generation++;
      stopTimers();
      state.list = [];
      state.listed = false;
      state.docs.clear();
      state.buttons.clear();
      state.active = "photos";
      ui.service.hidden = true;
      ui.photos.hidden = false;
      showTabs();
    },
  };
}
