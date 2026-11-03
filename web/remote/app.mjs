// Homepool photos, remote mode: pairs this phone with home through the relay
// and moves photos over one end-to-end encrypted session. Strings from links,
// files and the network only ever go into textContent.
import { formatBytes } from "../images/model.mjs";
import { MAX_FILE, Remote, get, list, put, status } from "./client.mjs";
import { deviceKeys, openStore, supported } from "./keys.mjs";
import { createServiceTabs } from "./tabs.mjs";
import {
  BlobCache,
  CancelledError,
  MAX_UNSETTLED,
  PRIORITY,
  TILE_LIMIT,
  fingerprint,
  imageType,
  kindLabel,
  newestFirst,
  parsePairing,
  randomId,
  UploadLog,
  uploadType,
} from "./model.mjs";

// Plain words for every outcome; no protocol terms, codes or raw errors.
const MESSAGES = {
  unreachable: "Can't reach the relay. Check that this phone is online, then try again.",
  unavailable: "Couldn't connect to home just now. Try again in a minute.",
  "home-offline":
    "Your pool at home isn't connected right now. Check that the home computer is on and online, then try again.",
  busy: "Too many devices are connected right now. Close this page on another phone, or try again in a minute.",
  interrupted: "The connection dropped. Try again.",
  timeout: "Home stopped answering. Try again.",
  store: "Home couldn't reach the photo storage just now. Try again in a minute.",
  refused: "Home didn't accept that request. Try again later.",
  "code-refused":
    "This pairing code didn't work. It may have expired (codes last 10 minutes) or already been used. Show a new code at home and scan it again.",
  malformed: "This link isn't a complete pairing code. Scan the code shown at home again.",
  "other-origin": "This pairing link is for a different address. Scan the code shown at home with this phone's camera.",
  unfinished: "Pairing didn't finish. Show a new code at home and scan it again.",
  removed: "This phone was removed at home, so it can't connect anymore. To use it again, pair it with a new code.",
  unrecognized:
    "Home didn't recognize this phone. If it was removed at home or home was set up again, pair it with a new code.",
  unsettled: `${MAX_UNSETTLED} earlier uploads aren't confirmed yet. Check or dismiss them above, then try again.`,
  storage:
    "This browser isn't letting the page save its pairing. Private browsing can do this; open the page in a normal tab.",
};
const say = (reason) => MESSAGES[reason] ?? "Something went wrong. Try again.";
const CONNECTION = new Set(["unreachable", "unavailable", "home-offline", "busy", "interrupted", "timeout"]);

const byId = (id) => document.getElementById(id);
const ui = Object.fromEntries(
  [
    "status", "refresh", "alert", "alert-text", "alert-action", "alert-close", "starting", "unsupported",
    "unsupported-text", "welcome", "welcome-note", "welcome-retry-row", "welcome-retry", "paste-form",
    "paste-text", "pairing", "step-connect", "step-check", "step-save", "paired", "paired-continue",
    "fingerprint", "replace", "replace-keep", "replace-accept", "library", "file-input", "keep-open", "uploads",
    "summary", "empty", "grid", "forget", "viewer", "viewer-close", "viewer-download", "viewer-image",
    "viewer-loading", "viewer-loading-text", "viewer-progress", "viewer-fallback", "viewer-kind",
    "viewer-fallback-text", "viewer-name", "viewer-meta", "tile-template", "upload-template",
    "heading", "tabs", "tab-photos", "photos", "service", "service-problem", "service-problem-text",
    "service-retry", "service-loading", "service-loading-text", "service-view", "service-state",
    "service-title", "service-age", "service-counts", "service-stale", "service-notes", "service-sections",
    "service-nothing",
  ].map((id) => [id.replace(/-(.)/g, (_, c) => c.toUpperCase()), byId(id)]),
);

// Read the pairing link once, then remove it from the address bar and this
// history entry before anything else runs. A link opened while the page is
// already open arrives as a hash change, not a new page load.
function takeFragment() {
  if (location.hash.length < 2) {
    if (location.hash) history.replaceState(null, "", location.pathname);
    return {};
  }
  const fragment = location.hash;
  history.replaceState(null, "", location.pathname);
  try {
    return { invite: parsePairing(fragment, location.origin) };
  } catch (error) {
    return { error: error.reason ?? "malformed" };
  }
}
// The link from the page load; dropped once it has been handed on.
let offered = takeFragment();
let started = null;
window.addEventListener("hashchange", async () => {
  const offer = takeFragment();
  if (!offer.invite && !offer.error) return;
  await started;
  if (ui.unsupported.hidden) receive(offer);
});

const store = openStore();
const keys = deviceKeys(store);
const uploadLog = new UploadLog(store);
const state = {
  generation: 0,
  objects: [],
  tiles: new Map(), // key -> tile record
  fetches: new Map(), // key -> queued or running download
  progress: new Map(), // key -> listener
  active: new Set(), // uploads that have not finished
  downloads: new Set(), // short-lived download URLs
  listing: null,
  viewing: null,
  opener: null,
  invite: null,
  removed: false,
};

const cache = new BlobCache({
  create: (blob) => URL.createObjectURL(blob),
  revoke: (url) => URL.revokeObjectURL(url),
  onEvict: (key) => evicted(key),
});

const remote = new Remote({
  origin: location.origin,
  store,
  keys,
  WebSocketImpl: WebSocket,
  onStatus: showStatus,
  onRemoved: (reason) => removed(reason),
});

// Service tabs beside Photos; the photo list loads only when Photos is shown.
const tabs = createServiceTabs({
  remote,
  ui,
  store,
  say: (reason) => say(reason),
  isLibrary: () => !ui.library.hidden && !state.removed,
  onPhotos: () => {
    if (!state.listed) refresh();
  },
});

// ---- small UI helpers -------------------------------------------------------

let alertAction = null;
let alertConnection = false;
function showAlert(text, { action, info = false, connection = false } = {}) {
  alertConnection = connection;
  ui.alertText.textContent = text;
  ui.alert.classList.toggle("info", info);
  alertAction = action ?? null;
  ui.alertAction.hidden = !action;
  if (action) ui.alertAction.textContent = action.label;
  ui.alert.hidden = false;
}
function clearAlert() {
  ui.alert.hidden = true;
  ui.alertText.textContent = "";
  alertAction = null;
}
ui.alertClose.addEventListener("click", clearAlert);
ui.alertAction.addEventListener("click", () => {
  const action = alertAction;
  clearAlert();
  action?.run();
});

function showStatus(value, reason) {
  // A connection problem clears itself once the page is connected again.
  if (value === "connected" && alertConnection) clearAlert();
  // A service tab says it in its own panel, with its own retry.
  if (
    value === "offline" &&
    !["removed", "unrecognized", "forgotten"].includes(reason) &&
    !ui.library.hidden &&
    tabs.active === "photos"
  )
    showAlert(say(reason), {
      connection: true,
      action: {
        label: "Try again",
        run: () => {
          remote.queue.resume();
          if (!state.listed) refresh();
        },
      },
    });
  const labels = { connected: "Connected", connecting: "Connecting…", offline: "Offline" };
  ui.status.hidden = !labels[value] || ui.library.hidden;
  ui.status.textContent = labels[value] ?? "";
  ui.status.className = `status ${value}`;
}

const PANELS = ["starting", "unsupported", "welcome", "pairing", "paired", "replace", "library"];
function showPanel(name) {
  for (const panel of PANELS) ui[panel].hidden = panel !== name;
  ui.refresh.hidden = name !== "library";
  if (name !== "library") ui.status.hidden = true;
}

function friendlyName(key) {
  const last = key.split("/").pop() ?? key;
  return /^\d{6}-[a-z2-7]{26}-(.+)$/.exec(last)?.[1] ?? last;
}

function addedAt(key) {
  const m = /^(\d{4}-\d{2}-\d{2})\/(\d{2})(\d{2})(\d{2})-/.exec(key);
  if (!m) return "";
  const date = new Date(`${m[1]}T${m[2]}:${m[3]}:${m[4]}Z`);
  return Number.isNaN(date.getTime()) ? "" : date.toLocaleString();
}

function failure(error, retry) {
  if (error instanceof CancelledError || error?.reason === "cancelled") return;
  const reason = error?.reason;
  if (reason === "removed" || reason === "unrecognized" || reason === "forgotten") return;
  showAlert(say(reason), retry ? { action: { label: "Try again", run: retry } } : {});
}

function current(generation) {
  return generation === state.generation;
}

// ---- pairing ---------------------------------------------------------------

function showWelcome(note, { retry = false } = {}) {
  ui.welcomeNote.hidden = !note;
  ui.welcomeNote.textContent = note ?? "";
  ui.welcomeRetryRow.hidden = !retry;
  ui.pasteText.value = "";
  showPanel("welcome");
}

function setStep(step) {
  const order = ["connecting", "checking", "saving"];
  const items = [ui.stepConnect, ui.stepCheck, ui.stepSave];
  const at = order.indexOf(step);
  items.forEach((item, i) => {
    item.classList.toggle("done", i < at);
    item.classList.toggle("active", i === at);
  });
}

async function pairWith(invite) {
  clearAlert();
  const hadDevice = Boolean(remote.device) && !state.removed;
  closeViewer();
  showPanel("pairing");
  setStep("connecting");
  try {
    const device = await remote.pair(invite, { onStep: setStep });
    paired(device);
  } catch (error) {
    if (error.reason === "forgotten") return;
    showAlert(say(error.reason));
    if (hadDevice) openLibrary();
    else showWelcome(state.removed ? say("removed") : null);
  }
}

function paired(device) {
  // The invitation is used up: drop every copy the page still holds.
  state.invite = null;
  ui.pasteText.value = "";
  state.removed = false;
  state.generation++;
  resetLibrary();
  ui.fingerprint.textContent = fingerprint(device.publicKey);
  showPanel("paired");
  // Best effort: ask the browser not to clear this site's storage.
  navigator.storage?.persist?.().catch(() => {});
}

ui.pairedContinue.addEventListener("click", () => {
  clearAlert();
  openLibrary();
});

ui.pasteForm.addEventListener("submit", (event) => {
  event.preventDefault();
  let invite;
  try {
    invite = parsePairing(ui.pasteText.value, location.origin);
  } catch (error) {
    showAlert(say(error.reason ?? "malformed"));
    return;
  }
  pairWith(invite);
});

ui.replaceKeep.addEventListener("click", () => {
  state.invite = null;
  clearAlert();
  openLibrary();
});
ui.replaceAccept.addEventListener("click", () => {
  const invite = state.invite;
  state.invite = null;
  pairWith(invite);
});

ui.welcomeRetry.addEventListener("click", async () => {
  clearAlert();
  ui.welcomeRetry.disabled = true;
  try {
    remote.queue.resume();
    await remote.ensure();
    state.removed = false;
    openLibrary();
  } catch (error) {
    if (!["removed", "unrecognized"].includes(error.reason)) failure(error);
  } finally {
    ui.welcomeRetry.disabled = false;
  }
});

function removed(reason) {
  state.removed = true;
  state.generation++;
  closeViewer();
  resetLibrary();
  showWelcome(say(reason), { retry: true });
}

// ---- library and grid ---------------------------------------------------------

function resetLibrary() {
  tabs.reset();
  remote.queue.clear();
  for (const tile of state.tiles.values()) visible.unobserve(tile.li);
  state.tiles.clear();
  state.fetches.clear();
  state.objects = [];
  ui.grid.replaceChildren();
  ui.uploads.replaceChildren();
  ui.summary.textContent = "";
  ui.empty.hidden = true;
  state.listed = false;
  cache.clear();
  updateKeepOpen();
}

function openLibrary() {
  showPanel("library");
  showStatus(remote.link && !remote.link.closed ? "connected" : "idle");
  tabs.start();
  restoreUploads();
}

function refresh() {
  if (state.listing) return state.listing;
  const generation = state.generation;
  remote.queue.resume();
  ui.refresh.disabled = true;
  if (!state.objects.length) ui.summary.textContent = "Loading your photos…";
  state.listing = (async () => {
    const items = [];
    let cursor = "";
    for (let pages = 0; pages < 500; pages++) {
      const page = await remote.run(PRIORITY.list, (link) => list(link, cursor)).done;
      if (!current(generation)) return;
      items.push(...page.items);
      cursor = page.cursor;
      if (!cursor) break;
      ui.summary.textContent = `Loading your photos… ${items.length}`;
    }
    state.objects = newestFirst(items);
    state.listed = true;
    renderGrid();
  })()
    .catch((error) => {
      if (!current(generation)) return;
      renderSummary();
      failure(error, refresh);
    })
    .finally(() => {
      state.listing = null;
      ui.refresh.disabled = false;
    });
  return state.listing;
}
ui.refresh.addEventListener("click", () => {
  clearAlert();
  if (tabs.active === "photos") refresh();
  else tabs.refresh();
});

function renderSummary() {
  const count = state.objects.length;
  const total = state.objects.reduce((sum, object) => sum + object.length, 0);
  ui.summary.textContent = count ? `${count} ${count === 1 ? "photo" : "photos"} · ${formatBytes(total)}` : "";
  ui.empty.hidden = count > 0 || !state.listed || state.active.size > 0;
}

function renderGrid() {
  const keep = new Set(state.objects.map((o) => o.key));
  for (const [key, tile] of state.tiles) {
    if (keep.has(key)) continue;
    visible.unobserve(tile.li);
    tile.job?.cancel();
    state.tiles.delete(key);
  }
  const order = state.objects.map((object) => state.tiles.get(object.key)?.li ?? makeTile(object).li);
  ui.grid.replaceChildren(...order);
  renderSummary();
}

function insertObject(object) {
  if (state.objects.some((o) => o.key === object.key)) return;
  state.objects = newestFirst([...state.objects, object]);
  renderGrid();
}

const visible = new IntersectionObserver(
  (entries) => {
    for (const entry of entries) {
      const tile = state.tiles.get(entry.target.dataset.key);
      if (!tile) continue;
      tile.visible = entry.isIntersecting;
      if (entry.isIntersecting) loadTile(tile);
      else leaveTile(tile);
    }
  },
  { rootMargin: "150px" },
);

function makeTile(object) {
  const fragment = ui.tileTemplate.content.cloneNode(true);
  const li = fragment.querySelector(".tile");
  li.dataset.key = object.key;
  const button = li.querySelector(".tile-button");
  li.querySelector(".tile-name").textContent = friendlyName(object.key);
  button.setAttribute("aria-label", friendlyName(object.key));
  button.addEventListener("click", () => openViewer(object, button));
  const tile = { li, object, img: li.querySelector("img"), job: null, visible: false, evicted: false };
  tile.img.addEventListener("load", () => li.classList.add("loaded"));
  tile.img.addEventListener("error", () => {
    if (tile.img.getAttribute("src")) fileTile(tile);
  });
  state.tiles.set(object.key, tile);
  visible.observe(li);
  return tile;
}

function fileTile(tile) {
  const { li, object } = tile;
  li.classList.add("file");
  li.classList.remove("loaded", "loading");
  tile.img.removeAttribute("src");
  const kind = li.querySelector(".kind");
  kind.textContent = kindLabel(object.key, object.type);
  kind.hidden = false;
  if (!li.querySelector(".tile-size")) {
    const size = document.createElement("span");
    size.className = "tile-size";
    size.textContent = formatBytes(object.length);
    li.querySelector(".tile-button").append(size);
  }
}

// Tiles load the original when they scroll into view, through the shared
// queue at the lowest priority; one that scrolls away before its turn is
// dropped from the queue.
function loadTile(tile) {
  const { object } = tile;
  if (tile.li.classList.contains("file")) return;
  if (!imageType(object.key, object.type) || object.length > TILE_LIMIT) return fileTile(tile);
  const cached = cache.get(object.key);
  if (cached) {
    tile.evicted = false;
    if (tile.img.getAttribute("src") !== cached.url) tile.img.src = cached.url;
    return;
  }
  if (tile.job) return;
  const generation = state.generation;
  tile.li.classList.add("loading");
  const job = fetchBlob(object.key, PRIORITY.tile);
  tile.job = job;
  job.done
    .then((entry) => {
      if (!current(generation)) return;
      tile.evicted = false;
      tile.img.src = entry.url;
    })
    .catch((error) => {
      if (!current(generation) || error instanceof CancelledError) return;
      // A connection problem leaves the tile to load again later.
      if (!CONNECTION.has(error.reason)) fileTile(tile);
    })
    .finally(() => {
      if (tile.job === job) tile.job = null;
      tile.li.classList.remove("loading");
    });
}

function leaveTile(tile) {
  if (tile.job && tile.job.priority === PRIORITY.tile && tile.job.cancel()) {
    tile.job = null;
    state.fetches.delete(tile.object.key);
    tile.li.classList.remove("loading");
  }
  if (tile.evicted) dropImage(tile);
}

function dropImage(tile) {
  tile.img.removeAttribute("src");
  tile.li.classList.remove("loaded");
  tile.evicted = false;
}

// A tile still on screen keeps showing its decoded image; it reloads only
// after it has left the screen and come back.
function evicted(key) {
  const tile = state.tiles.get(key);
  if (!tile) return;
  if (tile.visible) tile.evicted = true;
  else dropImage(tile);
}

// One download per key at a time, shared by its tile, the preview and the
// download button. The Blob is labelled with a fixed raster type, or as
// octet-stream, never with the stored type.
function fetchBlob(key, priority) {
  const existing = state.fetches.get(key);
  if (existing) {
    existing.promote(priority);
    return existing;
  }
  const object = state.objects.find((o) => o.key === key);
  const job = remote.run(priority, async (link) => {
    const cached = cache.get(key);
    if (cached) return cached;
    const { blob, mime } = await get(link, key, {
      onProgress: (fraction) => state.progress.get(key)?.(fraction),
    });
    const type = imageType(key, mime || object?.type) ?? "application/octet-stream";
    return cache.put(key, new Blob([blob], { type }));
  });
  state.fetches.set(key, job);
  const forget = () => {
    if (state.fetches.get(key) === job) state.fetches.delete(key);
  };
  job.done.then(forget, forget);
  return job;
}

// ---- preview and download --------------------------------------------------------

function viewerState(mode, text) {
  ui.viewerImage.hidden = mode !== "image";
  ui.viewerLoading.hidden = mode !== "loading";
  ui.viewerFallback.hidden = mode !== "file";
  if (mode === "file") ui.viewerFallbackText.textContent = text;
  if (mode === "loading") {
    ui.viewerLoadingText.textContent = text;
    ui.viewerProgress.value = 0;
  }
}

function openViewer(object, opener) {
  remote.queue.resume();
  const generation = state.generation;
  if (state.viewing) cache.unpin(state.viewing.key);
  state.viewing = object;
  state.opener = opener;
  ui.viewerName.textContent = friendlyName(object.key);
  const when = addedAt(object.key);
  ui.viewerMeta.textContent = `${formatBytes(object.length)}${when ? ` · added ${when}` : ""}`;
  ui.viewerKind.textContent = kindLabel(object.key, object.type);
  ui.viewerDownload.disabled = false;
  ui.viewerDownload.textContent = "Download";
  ui.viewerImage.removeAttribute("src");
  if (!ui.viewer.open) ui.viewer.showModal();
  document.body.classList.add("viewer-open");
  ui.viewerClose.focus();
  if (!imageType(object.key, object.type)) {
    viewerState("file", "This file can't be shown here. Download it to open it in another app.");
    return;
  }
  const show = (entry) => {
    if (!current(generation) || state.viewing !== object) return;
    cache.pin(object.key);
    ui.viewerImage.onload = () => viewerState("image");
    ui.viewerImage.onerror = () =>
      viewerState(
        "file",
        `This browser can't show ${kindLabel(object.key, object.type).toUpperCase()} photos. Download it to view it in another app.`,
      );
    ui.viewerImage.src = entry.url;
  };
  const cached = cache.get(object.key);
  if (cached) return show(cached);
  viewerState("loading", `Loading ${formatBytes(object.length)}…`);
  state.progress.set(object.key, (fraction) => {
    if (state.viewing === object) ui.viewerProgress.value = fraction;
  });
  updateKeepOpen();
  fetchBlob(object.key, PRIORITY.open)
    .done.then(show)
    .catch((error) => {
      if (!current(generation) || state.viewing !== object || error instanceof CancelledError) return;
      viewerState("file", `${say(error.reason)}`);
    })
    .finally(() => {
      state.progress.delete(object.key);
      updateKeepOpen();
    });
}

ui.viewerDownload.addEventListener("click", async () => {
  const object = state.viewing;
  if (!object) return;
  const generation = state.generation;
  remote.queue.resume();
  ui.viewerDownload.disabled = true;
  ui.viewerDownload.textContent = "Downloading…";
  state.progress.set(object.key, (fraction) => {
    if (state.viewing === object) ui.viewerDownload.textContent = `Downloading ${Math.round(fraction * 100)}%`;
  });
  updateKeepOpen();
  try {
    const entry = cache.get(object.key) ?? (await fetchBlob(object.key, PRIORITY.open).done);
    if (!current(generation)) return;
    // Always octet-stream: a download is never opened as a page document.
    const url = URL.createObjectURL(new Blob([entry.blob], { type: "application/octet-stream" }));
    state.downloads.add(url);
    const a = document.createElement("a");
    a.href = url;
    a.download = friendlyName(object.key);
    a.rel = "noopener";
    a.hidden = true;
    document.body.append(a);
    a.click();
    a.remove();
    setTimeout(() => {
      URL.revokeObjectURL(url);
      state.downloads.delete(url);
    }, 60000);
    ui.viewerDownload.textContent = "Downloaded";
  } catch (error) {
    if (!current(generation)) return;
    ui.viewerDownload.textContent = "Download";
    failure(error);
  } finally {
    state.progress.delete(object.key);
    ui.viewerDownload.disabled = false;
    updateKeepOpen();
  }
});

function closeViewer() {
  if (ui.viewer.open) ui.viewer.close();
}

ui.viewer.addEventListener("close", () => {
  const object = state.viewing;
  ui.viewerImage.removeAttribute("src");
  document.body.classList.remove("viewer-open");
  state.viewing = null;
  if (object) {
    cache.unpin(object.key);
    // A preview closed before its turn costs nothing.
    const job = state.fetches.get(object.key);
    if (job && !state.tiles.get(object.key)?.visible && job.cancel()) state.fetches.delete(object.key);
  }
  if (state.opener?.isConnected) state.opener.focus();
  state.opener = null;
});
ui.viewerClose.addEventListener("click", closeViewer);

// ---- uploads ------------------------------------------------------------------

function updateKeepOpen() {
  ui.keepOpen.hidden = state.active.size === 0 && state.progress.size === 0;
  renderSummary();
}

function uploadRow(name, file) {
  const fragment = ui.uploadTemplate.content.cloneNode(true);
  const item = fragment.querySelector(".upload");
  const thumb = item.querySelector(".upload-thumb");
  const row = {
    item,
    status: item.querySelector(".upload-status"),
    progress: item.querySelector(".upload-progress"),
    action: item.querySelector(".upload-action"),
    dismiss: item.querySelector(".upload-dismiss"),
    onAction: null,
    onDismiss: null,
  };
  item.querySelector(".upload-name").textContent = name;
  row.action.addEventListener("click", () => row.onAction?.());
  row.dismiss.addEventListener("click", () => {
    row.onDismiss?.();
    item.remove();
  });
  const type = file && imageType(file.name, file.type);
  if (type) {
    const url = URL.createObjectURL(file);
    const done = () => URL.revokeObjectURL(url);
    thumb.addEventListener("load", done, { once: true });
    thumb.addEventListener(
      "error",
      () => {
        done();
        thumb.removeAttribute("src");
        thumb.classList.add("blank");
      },
      { once: true },
    );
    thumb.src = url;
  } else thumb.classList.add("blank");
  ui.uploads.append(item);
  return row;
}

function rowState(row, kind, text, { action, dismiss = false } = {}) {
  row.item.classList.remove("done", "failed", "unsure", "checking");
  if (kind) row.item.classList.add(kind);
  row.status.textContent = text;
  row.action.hidden = !action;
  row.onAction = action?.run ?? null;
  if (action) row.action.textContent = action.label;
  row.dismiss.hidden = !dismiss;
}

// Begun uploads are remembered (ID, name, size only) so a reload, or iOS
// discarding the tab in the background, can still ask home what happened.
const settle = (id) => uploadLog.settle(id).catch(() => {});

function addFiles(files) {
  clearAlert();
  remote.queue.resume();
  for (const file of files) upload(file, uploadRow(file.name, file));
}

ui.fileInput.addEventListener("change", () => {
  addFiles([...ui.fileInput.files]);
  ui.fileInput.value = "";
});

// One request ID per File, made here and never reused for another file. A
// fresh ID is used only when the earlier attempt definitely did not store it.
function upload(file, row) {
  if (file.size > MAX_FILE) {
    rowState(row, "failed", `Too large: ${formatBytes(file.size)}. The pool takes files up to 50 MB from a phone.`, {
      dismiss: true,
    });
    return;
  }
  const generation = state.generation;
  const up = { id: randomId(), file, name: file.name, size: file.size, row, begun: false, accepted: false };
  state.active.add(up);
  updateKeepOpen();
  row.progress.value = 0;
  rowState(row, null, `${formatBytes(file.size)} · waiting`);
  const job = remote.run(
    PRIORITY.upload,
    (link) =>
      put(link, up, {
        // Nothing is sent if this throws: at the bound or on a storage error.
        onBegin: async () => {
          await uploadLog.remember(up);
          up.begun = true;
        },
        onProgress: (fraction) => {
          row.progress.value = fraction;
          row.status.textContent = `Uploading… ${Math.round(fraction * 100)}%`;
        },
        onEnd: () => {
          row.status.textContent = "Saving at home…";
        },
      }),
    { retry: false },
  );
  // Waiting uploads can be cancelled; a started one runs to its outcome.
  rowState(row, null, `${formatBytes(file.size)} · waiting`, {
    action: {
      label: "Cancel",
      run: () => {
        if (job.cancel()) row.item.remove();
      },
    },
  });
  job.onStart = () => rowState(row, null, "Starting…");
  if (job.started) job.onStart();
  job.done
    .then(({ key }) => {
      if (current(generation)) added(up, { key, length: file.size, type: uploadType(file.type) });
    })
    .catch((error) => {
      if (current(generation)) uploadFailed(up, error);
    })
    .finally(() => {
      state.active.delete(up);
      updateKeepOpen();
    });
}

function added(up, object) {
  const { row } = up;
  rowState(row, "done", `Added · ${formatBytes(object.length)}`);
  settle(up.id);
  const type = up.file && imageType(object.key, object.type);
  if (type && !cache.get(object.key)) cache.put(object.key, new Blob([up.file], { type }));
  insertObject(object);
  setTimeout(() => row.item.remove(), 4000);
}

function retryButton(up) {
  return up.file
    ? {
        label: "Upload again",
        run: () => {
          settle(up.id);
          upload(up.file, up.row);
        },
      }
    : null;
}

function uploadFailed(up, error) {
  const { row } = up;
  if (error instanceof CancelledError || error?.reason === "forgotten") return;
  if (["removed", "unrecognized"].includes(error?.reason)) return;
  if (!up.begun) {
    if (error.reason === "unreadable")
      return rowState(row, "failed", "This photo couldn't be read from the phone. Pick it again.", { dismiss: true });
    return rowState(row, "failed", `Not sent. ${say(error.reason)}`, { action: retryButton(up), dismiss: true });
  }
  if (error.reason === "unreadable") {
    // The page stopped sending; home will not store a short file.
    return check(up, "This photo couldn't be read from the phone. Checking with home…");
  }
  if (["refused", "store"].includes(error.reason) && !up.accepted) {
    settle(up.id);
    return rowState(row, "failed", "Home didn't take this photo. It may be over today's upload limit; try again later.", {
      action: retryButton(up),
      dismiss: true,
    });
  }
  check(up, "Interrupted. Checking with home…");
}

// Asks home what happened to an upload it may have received: never claims a
// result it was not told, and offers a fresh upload only when it failed.
function check(up, text = "Checking with home…") {
  const { row } = up;
  const generation = state.generation;
  // Dismissing an unconfirmed upload is the owner's call to stop checking it.
  row.onDismiss = () => settle(up.id);
  rowState(row, "checking", text);
  row.progress.value = 0;
  remote
    .run(PRIORITY.list, (link) => status(link, up.id))
    .done.then((m) => {
      if (!current(generation)) return;
      if (m.type === "committed") return added(up, { key: m.key, length: m.length, type: m.mime });
      if (m.type === "failed") {
        settle(up.id);
        if (up.file) return rowState(row, "failed", "Upload failed.", { action: retryButton(up), dismiss: true });
        return rowState(row, "failed", "Upload failed. Pick the photo again to add it.", { dismiss: true });
      }
      if (m.type === "pending")
        return rowState(row, "unsure", "Not confirmed yet. Home may still be saving it, or lost contact.", {
          action: { label: "Check again", run: () => check(up) },
          dismiss: true,
        });
      rowState(row, "unsure", "Not confirmed. Home has no record of it. Check the photo list; if it's missing, add it again.", {
        action: {
          label: "Check the list",
          run: () => {
            settle(up.id);
            row.item.remove();
            refresh();
          },
        },
        dismiss: true,
      });
    })
    .catch((error) => {
      if (!current(generation) || ["removed", "unrecognized"].includes(error?.reason)) return;
      rowState(row, "unsure", `Not confirmed. ${say(error.reason)}`, {
        action: { label: "Check again", run: () => (remote.queue.resume(), check(up)) },
        dismiss: true,
      });
    });
}

let restored = false;
async function restoreUploads() {
  if (restored) return;
  restored = true;
  const known = new Set([...state.active].map((u) => u.id));
  for (const entry of (await uploadLog.entries().catch(() => null)) ?? []) {
    if (known.has(entry.id)) continue;
    const row = uploadRow(entry.name);
    const up = { id: entry.id, file: null, name: entry.name, size: entry.size, row, begun: true, accepted: true };
    check(up, "Interrupted while the page was closed. Checking with home…");
  }
}

// ---- forget ---------------------------------------------------------------------

ui.forget.addEventListener("click", async () => {
  if (
    !confirm(
      "Forget the pairing on this phone?\n\nYour photos stay at home. To use this phone again, you'll need a new pairing code.",
    )
  )
    return;
  state.generation++;
  closeViewer();
  for (const up of state.active) up.row.item.remove();
  state.active.clear();
  state.progress.clear();
  try {
    await remote.forget();
  } catch {}
  resetLibrary();
  for (const url of state.downloads) URL.revokeObjectURL(url);
  state.downloads.clear();
  restored = false;
  state.removed = false;
  state.invite = null;
  ui.pasteText.value = "";
  clearAlert();
  showWelcome(
    "This phone is no longer paired, and its pairing was deleted from this browser. Your photos at home are untouched. To also remove it from the list at home, remove it there.",
  );
});

// ---- visibility ---------------------------------------------------------------

// Coming back to the library reconnects once, so it is live without a tap;
// there are still no idle pings, and the idle close applies as usual.
document.addEventListener("visibilitychange", () => {
  if (document.hidden) {
    tabs.hidden();
    return remote.hidden();
  }
  const library = !ui.library.hidden && !state.removed;
  remote.visible({ reconnect: library });
  if (library && tabs.active === "photos" && !state.listed) refresh();
  if (library) tabs.visible();
});

// ---- start ---------------------------------------------------------------------

async function start() {
  if (!(await supported())) {
    showPanel("unsupported");
    return;
  }
  let device;
  let pending;
  try {
    device = await remote.load();
    pending = await store.get("pending");
  } catch {
    ui.unsupportedText.textContent = say("storage");
    showPanel("unsupported");
    return;
  }
  const offer = offered;
  offered = null;
  if (offer.invite || offer.error) {
    if (!device) showWelcome();
    else openLibrary();
    return receive(offer);
  }
  if (pending) {
    // A pairing was interrupted after its last step; ask home once more.
    showPanel("pairing");
    setStep("saving");
    try {
      paired(await remote.recoverPairing());
      return;
    } catch (error) {
      if (!CONNECTION.has(error.reason)) await store.del("pending").catch(() => {});
      showAlert(CONNECTION.has(error.reason) ? say(error.reason) : say("unfinished"));
    }
  }
  if (device) openLibrary();
  else showWelcome();
}

// A pairing link: a broken one is refused with the current state kept; a
// valid one pairs, or first asks when this phone is already paired.
function receive({ invite, error }) {
  if (error) return showAlert(say(error));
  closeViewer();
  if (remote.device && !state.removed) {
    state.invite = invite;
    showPanel("replace");
  } else pairWith(invite);
}

started = start();
