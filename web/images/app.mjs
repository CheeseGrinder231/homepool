// Homepool image page: one bucket, signed in the browser, same origin as the
// Caddy site that proxies the bucket. Strings from the network (keys, error
// text) are only ever assigned to textContent.
import {
  RANDOM_TAG_BYTES,
  createConnection,
  displayName,
  extensionOf,
  formatBytes,
  looksLikeImage,
  objectKey,
  parseListing,
  parseSetup,
  randomTag,
  setupFragment,
  sortNewestFirst,
} from "./model.mjs";
import { EMPTY_SHA256, canonicalPath, presignUrl, sha256Hex, signHeaders, uriEncode } from "./sigv4.mjs";

const STORAGE_KEY = "homepool-images.setup";
const PREVIEW_SECONDS = 600;
const DOWNLOAD_SECONDS = 300;
const LIST_PAGE = "1000";

const byId = (id) => document.getElementById(id);
const ui = {
  alert: byId("alert"),
  alertText: byId("alert-text"),
  alertClose: byId("alert-close"),
  replace: byId("replace"),
  replaceDetail: byId("replace-detail"),
  replaceAccept: byId("replace-accept"),
  replaceKeep: byId("replace-keep"),
  setup: byId("setup"),
  setupForm: byId("setup-form"),
  setupText: byId("setup-text"),
  library: byId("library"),
  fileInput: byId("file-input"),
  uploads: byId("uploads"),
  summary: byId("summary"),
  empty: byId("empty"),
  grid: byId("grid"),
  refresh: byId("refresh"),
  forget: byId("forget"),
  viewer: byId("viewer"),
  viewerClose: byId("viewer-close"),
  viewerDownload: byId("viewer-download"),
  viewerImage: byId("viewer-image"),
  viewerFallback: byId("viewer-fallback"),
  viewerFallbackText: byId("viewer-fallback-text"),
  viewerKind: byId("viewer-kind"),
  viewerName: byId("viewer-name"),
  viewerMeta: byId("viewer-meta"),
  tileTemplate: byId("tile-template"),
  uploadTemplate: byId("upload-template"),
};

const state = { config: null, objects: [], queue: [], uploading: false, offered: null, viewing: null, opener: null };

function showAlert(message) {
  ui.alertText.textContent = message;
  ui.alert.hidden = false;
}

function clearAlert() {
  ui.alert.hidden = true;
  ui.alertText.textContent = "";
}

function loadConfig() {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    return stored ? parseSetup(stored) : null;
  } catch {
    return null;
  }
}

function sameConfig(a, b) {
  return Boolean(a && b) && setupFragment(a) === setupFragment(b);
}

// The fragment leaves the address bar before anything else happens, whether
// or not it parses. Nothing is stored here: a link only replaces a working
// key after the user accepts it and the new key has listed the bucket.
function takeSetupFromFragment() {
  const fragment = location.hash;
  if (fragment.length < 2) return null;
  history.replaceState(null, "", location.pathname + location.search);
  try {
    return parseSetup(fragment);
  } catch (error) {
    showAlert(error.message);
    return null;
  }
}

function credentials(config = state.config) {
  return { ...config, service: "s3" };
}

function bucketPath(key, config = state.config) {
  return key === undefined ? `/${config.bucket}` : `/${config.bucket}/${key}`;
}

function requestUrl(path, query = []) {
  const search = query.map(([name, value]) => `${uriEncode(name)}=${uriEncode(value)}`).join("&");
  return `${location.origin}${canonicalPath(path)}${search ? `?${search}` : ""}`;
}

async function s3(method, path, { query = [], headers = {}, contentSha256 = EMPTY_SHA256, config = state.config } = {}) {
  const signed = await signHeaders({ method, host: location.host, path, query, headers, contentSha256 }, credentials(config));
  return fetch(requestUrl(path, query), {
    method,
    headers: signed.headers,
    cache: "no-store",
    credentials: "omit",
    referrerPolicy: "no-referrer",
  });
}

function httpError(what, response) {
  return new Error(`${what} failed: HTTP ${response.status}${response.statusText ? ` ${response.statusText}` : ""}.`);
}

// The query shapes here are the ones the Caddy site allows; keep them in step.
async function listAll(config = state.config) {
  const objects = [];
  let token;
  do {
    const query = [["list-type", "2"], ["max-keys", LIST_PAGE]];
    if (token) query.push(["continuation-token", token]);
    const response = await s3("GET", bucketPath(undefined, config), { query, config });
    if (!response.ok) throw httpError("Listing", response);
    const listing = parseListing(await response.text());
    objects.push(...listing.objects);
    token = listing.truncated ? listing.continuationToken : undefined;
  } while (token);
  return sortNewestFirst(objects);
}

function presign(key, seconds) {
  return presignUrl(location.origin, location.host, bucketPath(key), credentials(), seconds);
}

const visible = new IntersectionObserver(
  (entries) => {
    for (const entry of entries) {
      if (!entry.isIntersecting) continue;
      visible.unobserve(entry.target);
      loadTile(entry.target);
    }
  },
  { rootMargin: "200px" },
);

const tiles = new Map();

function fileTile(tile, object) {
  tile.classList.add("file");
  tile.classList.remove("loaded");
  const kind = tile.querySelector(".kind");
  kind.textContent = extensionOf(object.key) || "file";
  kind.hidden = false;
}

async function loadTile(tile) {
  const object = tiles.get(tile);
  if (!looksLikeImage(object.key)) {
    fileTile(tile, object);
    return;
  }
  const image = tile.querySelector("img");
  image.addEventListener("load", () => tile.classList.add("loaded"));
  image.addEventListener("error", async () => {
    // One retry covers a presigned URL that expired while the phone slept.
    if (tile.dataset.retried) {
      fileTile(tile, object);
      return;
    }
    tile.dataset.retried = "1";
    image.src = await presign(object.key, PREVIEW_SECONDS);
  });
  image.src = await presign(object.key, PREVIEW_SECONDS);
}

function makeTile(object) {
  const fragment = ui.tileTemplate.content.cloneNode(true);
  const tile = fragment.querySelector(".tile");
  tile.querySelector(".tile-name").textContent = displayName(object.key);
  tile.querySelector(".tile-button").setAttribute("aria-label", displayName(object.key));
  tile.querySelector(".tile-button").addEventListener("click", (event) => openViewer(object, event.currentTarget));
  tiles.set(tile, object);
  visible.observe(tile);
  return tile;
}

function renderSummary() {
  const count = state.objects.length;
  const total = state.objects.reduce((sum, object) => sum + object.size, 0);
  ui.summary.textContent = count ? `${count} ${count === 1 ? "file" : "files"} · ${formatBytes(total)}` : "";
  ui.empty.hidden = count > 0;
}

function renderGrid() {
  ui.grid.replaceChildren();
  tiles.clear();
  for (const object of state.objects) ui.grid.append(makeTile(object));
  renderSummary();
}

function insertObject(object) {
  state.objects = sortNewestFirst([...state.objects, object]);
  const index = state.objects.indexOf(object);
  const tile = makeTile(object);
  const next = ui.grid.children[index];
  if (next) next.before(tile);
  else ui.grid.append(tile);
  renderSummary();
}

async function refresh() {
  ui.refresh.disabled = true;
  ui.summary.textContent = "Loading…";
  try {
    state.objects = await listAll();
    renderGrid();
  } catch (error) {
    showAlert(error.message);
    ui.summary.textContent = "";
  } finally {
    ui.refresh.disabled = false;
  }
}

function putObject(key, file, buffer, hash, onProgress) {
  return new Promise((resolve, reject) => {
    signHeaders(
      {
        method: "PUT",
        host: location.host,
        path: bucketPath(key),
        headers: { "content-type": file.type || "application/octet-stream" },
        contentSha256: hash,
      },
      credentials(),
    ).then((signed) => {
      const xhr = new XMLHttpRequest();
      xhr.open("PUT", requestUrl(bucketPath(key)));
      for (const [name, value] of Object.entries(signed.headers)) xhr.setRequestHeader(name, value);
      xhr.upload.addEventListener("progress", (event) => {
        if (event.lengthComputable) onProgress(event.loaded / event.total);
      });
      xhr.addEventListener("load", () => {
        if (xhr.status >= 200 && xhr.status < 300) resolve();
        else reject(new Error(`Upload failed: HTTP ${xhr.status}.`));
      });
      xhr.addEventListener("error", () => reject(new Error("Upload failed: the connection dropped.")));
      xhr.addEventListener("abort", () => reject(new Error("Upload cancelled.")));
      xhr.send(buffer);
    }, reject);
  });
}

// Garage has no create-only PUT, so the key's 128 random bits are what keeps
// one upload from replacing another; nothing here checks first.
async function uploadOne(file, row) {
  row.status.textContent = "Preparing…";
  const buffer = await file.arrayBuffer();
  const hash = await sha256Hex(buffer);
  const key = objectKey(file.name, new Date(), randomTag(crypto.getRandomValues(new Uint8Array(RANDOM_TAG_BYTES))));
  row.status.textContent = "Uploading…";
  await putObject(key, file, buffer, hash, (fraction) => {
    row.progress.value = fraction;
    row.status.textContent = `Uploading… ${Math.round(fraction * 100)}%`;
  });
  return { key, size: file.size, lastModified: new Date().toISOString() };
}

function addUploadRow(file) {
  const fragment = ui.uploadTemplate.content.cloneNode(true);
  const item = fragment.querySelector(".upload");
  const thumb = item.querySelector(".upload-thumb");
  const row = {
    item,
    status: item.querySelector(".upload-status"),
    progress: item.querySelector(".upload-progress"),
    retry: item.querySelector(".upload-retry"),
  };
  item.querySelector(".upload-name").textContent = file.name;
  row.status.textContent = `${formatBytes(file.size)} · waiting`;
  if (file.type.startsWith("image/")) {
    const url = URL.createObjectURL(file);
    thumb.addEventListener("load", () => URL.revokeObjectURL(url), { once: true });
    thumb.addEventListener(
      "error",
      () => {
        URL.revokeObjectURL(url);
        thumb.removeAttribute("src");
        thumb.classList.add("blank");
      },
      { once: true },
    );
    thumb.src = url;
  } else {
    thumb.classList.add("blank");
  }
  ui.uploads.append(item);
  return row;
}

async function runQueue() {
  if (state.uploading) return;
  state.uploading = true;
  while (state.queue.length) {
    const { file, row } = state.queue.shift();
    try {
      const object = await uploadOne(file, row);
      row.item.classList.add("done");
      row.status.textContent = `Added · ${formatBytes(file.size)}`;
      insertObject(object);
      setTimeout(() => row.item.remove(), 4000);
    } catch (error) {
      row.item.classList.add("failed");
      row.status.textContent = error.message;
      row.retry.hidden = false;
      row.retry.addEventListener(
        "click",
        () => {
          row.item.classList.remove("failed");
          row.retry.hidden = true;
          row.progress.value = 0;
          state.queue.push({ file, row });
          runQueue();
        },
        { once: true },
      );
    }
  }
  state.uploading = false;
}

function enqueueFiles(files) {
  clearAlert();
  for (const file of files) state.queue.push({ file, row: addUploadRow(file) });
  runQueue();
}

async function openViewer(object, opener) {
  state.viewing = object;
  state.opener = opener;
  const name = displayName(object.key);
  ui.viewerName.textContent = name;
  ui.viewerMeta.textContent = `${formatBytes(object.size)}${object.lastModified ? ` · ${new Date(object.lastModified).toLocaleString()}` : ""}`;
  ui.viewerDownload.setAttribute("download", name);
  ui.viewerDownload.dataset.signed = "";
  ui.viewerFallback.hidden = true;
  ui.viewerImage.hidden = true;
  ui.viewerImage.removeAttribute("src");
  ui.viewerKind.textContent = extensionOf(object.key) || "file";
  if (!ui.viewer.open) ui.viewer.showModal();
  ui.viewerClose.focus();
  document.body.classList.add("viewer-open");
  if (!looksLikeImage(object.key)) {
    ui.viewerFallbackText.textContent = "This is not an image file. Download it to open it elsewhere.";
    ui.viewerFallback.hidden = false;
    return;
  }
  ui.viewerImage.onerror = () => {
    ui.viewerImage.hidden = true;
    ui.viewerFallbackText.textContent = `This browser cannot show ${extensionOf(object.key).toUpperCase()} files. Download it to view it in another app.`;
    ui.viewerFallback.hidden = false;
  };
  ui.viewerImage.onload = () => {
    ui.viewerImage.hidden = false;
  };
  ui.viewerImage.src = await presign(object.key, PREVIEW_SECONDS);
}

// The download URL is signed when the link is activated, so a viewer left
// open past the five-minute window still downloads.
ui.viewerDownload.addEventListener("click", async (event) => {
  if (ui.viewerDownload.dataset.signed === "1") {
    ui.viewerDownload.dataset.signed = "";
    return;
  }
  event.preventDefault();
  if (!state.viewing) return;
  ui.viewerDownload.href = await presign(state.viewing.key, DOWNLOAD_SECONDS);
  ui.viewerDownload.dataset.signed = "1";
  ui.viewerDownload.click();
});

function closeViewer() {
  if (ui.viewer.open) ui.viewer.close();
}

ui.viewer.addEventListener("close", () => {
  ui.viewerImage.removeAttribute("src");
  ui.viewerDownload.setAttribute("href", "#");
  ui.viewerDownload.dataset.signed = "";
  document.body.classList.remove("viewer-open");
  state.viewing = null;
  if (state.opener?.isConnected) state.opener.focus();
  state.opener = null;
});

function showPanel(panel) {
  for (const section of [ui.replace, ui.setup, ui.library]) section.hidden = section !== panel;
  ui.refresh.hidden = panel !== ui.library;
}

function showLibrary(objects) {
  showPanel(ui.library);
  if (objects) {
    state.objects = objects;
    renderGrid();
  } else {
    refresh();
  }
}

function showSetup() {
  showPanel(ui.setup);
  ui.setupText.value = "";
}

// Lists the bucket with the offered key before anything is stored. A key
// that cannot list leaves the current configuration, if any, untouched, and
// so does a listing that finishes after Keep, Forget or a newer attempt.
const connection = createConnection({
  list: listAll,
  store(config) {
    state.config = config;
    localStorage.setItem(STORAGE_KEY, setupFragment(config));
  },
});

async function connect(config) {
  const objects = await connection.connect(config);
  if (objects) showLibrary(objects);
}

function offerReplacement(offered) {
  state.offered = offered;
  ui.replaceDetail.textContent = `New key ${offered.accessKeyId} for bucket ${offered.bucket}`;
  showPanel(ui.replace);
}

ui.replaceAccept.addEventListener("click", async () => {
  const offered = state.offered;
  state.offered = null;
  clearAlert();
  ui.replaceAccept.disabled = true;
  try {
    await connect(offered);
  } catch (error) {
    showAlert(`The new key was not used: ${error.message} The current key is kept.`);
    if (state.config) showLibrary();
  } finally {
    ui.replaceAccept.disabled = false;
  }
});

ui.replaceKeep.addEventListener("click", () => {
  connection.cancel();
  state.offered = null;
  showLibrary();
});

ui.setupForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  clearAlert();
  try {
    await connect(parseSetup(ui.setupText.value));
  } catch (error) {
    showAlert(error.message);
  }
});

ui.fileInput.addEventListener("change", () => {
  enqueueFiles([...ui.fileInput.files]);
  ui.fileInput.value = "";
});

ui.refresh.addEventListener("click", () => {
  clearAlert();
  refresh();
});

ui.forget.addEventListener("click", () => {
  if (!confirm("Forget the pool key on this phone? You will need the setup code again.")) return;
  connection.cancel();
  localStorage.removeItem(STORAGE_KEY);
  clearAlert();
  state.config = null;
  state.objects = [];
  ui.grid.replaceChildren();
  closeViewer();
  showSetup();
});

ui.alertClose.addEventListener("click", clearAlert);
ui.viewerClose.addEventListener("click", closeViewer);

async function start() {
  if (!window.isSecureContext || !crypto.subtle) {
    showAlert("This page needs a secure (HTTPS) connection to sign requests.");
    return;
  }
  const stored = loadConfig();
  const offered = takeSetupFromFragment();
  state.config = stored;
  if (offered && stored && !sameConfig(offered, stored)) {
    offerReplacement(offered);
  } else if (offered && !stored) {
    try {
      await connect(offered);
    } catch (error) {
      showAlert(error.message);
      showSetup();
    }
  } else if (stored) {
    showLibrary();
  } else {
    showSetup();
  }
}

start();
