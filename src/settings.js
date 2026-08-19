// Settings page. Reads/writes persisted settings over IPC; the backend
// broadcasts `settings-changed` so the chrome + other pages update live.

import { applyAppearance } from "./appearance.js";
import { invoke, listen } from "./ipc.js";
import { PRESETS, hostOf, paintFavicon, wallpaperCss } from "./util.js";

// Curated accent palette + a custom picker.
const ACCENTS = [
  "#d97b32", "#5b9dff", "#3fc7c0", "#57c785",
  "#e0b23a", "#ff6b6b", "#8b93a7", "#a78bfa",
];

const UA_PRESETS = {
  safari:
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.4 Safari/605.1.15",
  chrome:
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36",
  firefox: "Mozilla/5.0 (Macintosh; Intel Mac OS X 14.4; rv:138.0) Gecko/20100101 Firefox/138.0",
  mobile:
    "Mozilla/5.0 (iPhone; CPU iPhone OS 18_4 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.4 Mobile/15E148 Safari/604.1",
};

const TRASH = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18M8 6V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2m2 0v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"/><path d="M10 11v6M14 11v6"/></svg>`;

const $ = (id) => document.getElementById(id);
const fmtMB = (b) => `${Math.round(b / 1048576)} MB`;

let settings = null;

// ---- toast -------------------------------------------------------------------
let toastTimer;
function toast(msg = "Saved") {
  const el = $("saved");
  el.textContent = msg;
  el.classList.add("show");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.remove("show"), 1400);
}

async function save() {
  await invoke("set_settings", { settings });
  applyAppearance(settings);
  toast();
}

// ---- appearance --------------------------------------------------------------
function renderTheme() {
  for (const b of $("theme").querySelectorAll("button")) {
    b.classList.toggle("active", b.dataset.v === settings.theme);
  }
}

function renderAccents() {
  const wrap = $("accents");
  const dots = ACCENTS.map((hex) => {
    const b = document.createElement("button");
    b.className = "accent-dot";
    b.style.background = hex;
    b.title = hex;
    if ((settings.accent || "").toLowerCase() === hex.toLowerCase()) {
      b.classList.add("sel");
    }
    b.addEventListener("click", () => setAccent(hex));
    return b;
  });
  // Custom colour picker, seeded with the current accent.
  const custom = document.createElement("input");
  custom.type = "color";
  custom.className = "accent-custom";
  custom.title = "Custom colour";
  custom.value = settings.accent || "#d97b32";
  custom.addEventListener("input", (e) => {
    settings.accent = e.target.value;
    applyAppearance(settings); // live preview
  });
  custom.addEventListener("change", (e) => setAccent(e.target.value));
  wrap.replaceChildren(...dots, custom);
}

async function setAccent(hex) {
  settings.accent = hex;
  renderAccents();
  await save();
}

function renderSwatches() {
  const wp = settings.wallpaper;
  $("swatches").replaceChildren(
    ...Object.entries(PRESETS).map(([id, css]) => {
      const el = document.createElement("div");
      el.className = "swatch";
      el.style.background = css;
      el.title = id[0].toUpperCase() + id.slice(1);
      if (wp.kind === "preset" && wp.value === id) el.classList.add("sel");
      el.addEventListener("click", () => setWallpaper("preset", id));
      return el;
    })
  );
  // Live preview of whatever the home page currently shows.
  $("wppreview").style.background = wallpaperCss(wp);
  $("wptag").textContent =
    wp.kind === "preset"
      ? `Preset · ${wp.value}`
      : wp.kind === "image"
      ? "Uploaded image"
      : "Image URL";
  $("wpurl").value = wp.kind === "url" ? wp.value : "";
}

async function setWallpaper(kind, value) {
  await invoke("set_wallpaper", { kind, value });
  // Re-read: uploaded images come back as a version tag, not the data URL.
  const wp = (await invoke("get_settings")).wallpaper;
  if (kind === "image" && wp.kind !== "image") {
    toast("Unsupported image (use PNG, JPEG, WebP, GIF or AVIF)");
    return;
  }
  settings.wallpaper = wp;
  renderSwatches();
  toast("Wallpaper updated");
}

async function initTransparencyRow() {
  const sys = await invoke("system_reduce_transparency").catch(() => false);
  if (sys) {
    $("rtdesc").textContent =
      "Your system accessibility setting already reduces transparency, so Foxlite runs opaque.";
    $("reducetransparency").disabled = true;
    $("reducetransparency").checked = true;
  }
}

// ---- bookmarks manager -------------------------------------------------------
async function loadBookmarks() {
  const list = await invoke("get_bookmarks");
  const el = $("bmlist");
  $("bm-count").textContent = list.length
    ? `${list.length} bookmark${list.length === 1 ? "" : "s"}.`
    : "Manage the pages you've starred.";

  if (list.length === 0) {
    el.replaceChildren(
      Object.assign(document.createElement("div"), {
        className: "muted",
        textContent: "No bookmarks yet — tap the ★ in the toolbar to add one.",
      })
    );
    return;
  }

  el.replaceChildren(
    ...list.map((b) => {
      const row = document.createElement("div");
      row.className = "bm-item";

      const fav = document.createElement("span");
      fav.className = "tab-fav";
      paintFavicon(fav, b.favicon);

      const meta = document.createElement("div");
      meta.className = "meta";
      const t = document.createElement("div");
      t.className = "t";
      t.textContent = b.title || hostOf(b.url);
      const u = document.createElement("div");
      u.className = "u";
      u.textContent = b.url;
      meta.append(t, u);

      const del = document.createElement("button");
      del.className = "bm-del";
      del.title = "Remove bookmark";
      del.innerHTML = TRASH;
      del.addEventListener("click", async () => {
        await invoke("remove_bookmark", { url: b.url });
        loadBookmarks();
        toast("Bookmark removed");
      });

      row.append(fav, meta, del);
      return row;
    })
  );
}

// ---- performance -------------------------------------------------------------
async function refreshMemory() {
  const el = $("memdesc");
  try {
    const r = await invoke("memory_report");
    if (!r.available) {
      el.textContent = "Per-tab memory is shown on macOS.";
      return;
    }
    const n = r.tabs.length;
    el.replaceChildren();
    const head = document.createElement("div");
    head.className = "memtotal";
    const total = document.createElement("b");
    total.textContent = fmtMB(r.total_bytes);
    head.append(
      total,
      ` total · app + UI ${fmtMB(r.app_bytes)} · ${n} loaded tab${n === 1 ? "" : "s"}`
    );
    el.appendChild(head);
    if (n) {
      const list = document.createElement("div");
      list.className = "memlist";
      for (const t of r.tabs.sort((a, b) => b.bytes - a.bytes)) {
        const span = document.createElement("span");
        const b = document.createElement("b");
        b.textContent = fmtMB(t.bytes);
        span.append(b, ` ${t.title.length > 28 ? t.title.slice(0, 27) + "…" : t.title}`);
        list.appendChild(span);
      }
      el.appendChild(list);
    }
  } catch {
    el.textContent = "Memory info unavailable.";
  }
}

async function initBlocker() {
  const [info, status] = await Promise.all([
    invoke("blocklist_info"),
    invoke("blocker_status").catch(() => "unsupported"),
  ]);
  const n = info.rules.toLocaleString();
  const chips = $("aboutchips");
  for (const t of ["WKWebView", `${n} blocking rules`, "Zero-dependency store", "Native menus"]) {
    const c = document.createElement("span");
    c.className = "chip";
    c.textContent = t;
    chips.appendChild(c);
  }
  const byCat = (cat) => info.sources.filter((s) => s.category === cat).map((s) => s.name).join(", ");
  const cnt = (cat) => (info.categories?.[cat]?.rules ?? 0).toLocaleString();
  $("blockdesc").textContent =
    `${byCat("ads")} + ${byCat("privacy")} (${cnt("ads")} + ${cnt("privacy")} rules, lists dated ${info.generated}) — ` +
    "the same filter lists uBlock Origin uses, compiled into WebKit's content blocker so ads and trackers are never " +
    "requested and empty ad slots are hidden. Applies on the next page load.";
  $("malwaredesc").textContent =
    `${cnt("security")} known malware and phishing hosts (${byCat("security")}) are never contacted, and WebKit's ` +
    "fraudulent-site warning (Safe Browsing) is shown before loading flagged pages.";
  $("annoydesc").textContent =
    `${cnt("annoyances")} rules from ${byCat("annoyances")} remove cookie-consent overlays and similar nag boxes. ` +
    "Turn off if a site misbehaves.";
  const msg = {
    ready: "",
    compiling: "Compiling the rule list…",
    failed: "The blocker could not be compiled on this system, so nothing is blocked.",
    unsupported: "Content blocking is only available on macOS for now.",
  }[status];
  $("blockstatus").textContent = msg || "";
  $("blockstatus").hidden = !msg;
  if (status === "failed" || status === "unsupported") {
    for (const id of ["blockads", "blockmalware", "blockannoyances"]) $(id).disabled = true;
  }
}

// ---- developer ---------------------------------------------------------------
function loadUA(ua) {
  const preset = Object.entries(UA_PRESETS).find(([, v]) => v === ua)?.[0];
  $("uapreset").value = ua ? preset || "custom" : "";
  $("ua").value = ua;
  $("uarow").hidden = $("uapreset").value !== "custom";
}

// ---- wiring ------------------------------------------------------------------
/// Bind a control to a settings field: `read` turns the change event into the
/// stored value.
function bind(id, event, field, read) {
  $(id).addEventListener(event, (e) => {
    settings[field] = read(e.target);
    save();
  });
}

function wireControls() {
  $("theme").addEventListener("click", (e) => {
    const v = e.target.dataset.v;
    if (!v) return;
    settings.theme = v;
    renderTheme();
    save();
  });
  bind("engine", "change", "search_engine", (t) => t.value);
  bind("zoom", "change", "default_zoom", (t) => parseFloat(t.value));
  bind("bmbar", "change", "show_bookmarks_bar", (t) => t.checked);
  bind("verticaltabs", "change", "vertical_tabs", (t) => t.checked);
  bind("mrutabs", "change", "mru_tab_switching", (t) => t.checked);
  bind("discard", "change", "discard_after_min", (t) => parseInt(t.value, 10));
  bind("blockads", "change", "block_ads", (t) => t.checked);
  bind("blockmalware", "change", "block_malware", (t) => t.checked);
  bind("blockpopups", "change", "block_popups", (t) => t.checked);
  bind("blockannoyances", "change", "block_annoyances", (t) => t.checked);
  bind("startup", "change", "startup", (t) => t.value);
  bind("devtools", "change", "devtools", (t) => t.checked);
  bind("ua", "change", "user_agent", (t) => t.value.trim());
  $("reducetransparency").addEventListener("change", async (e) => {
    settings.reduce_transparency = e.target.checked;
    await save();
    toast("Takes effect after restarting Foxlite");
  });

  $("wpurl").addEventListener("change", (e) => {
    const v = e.target.value.trim();
    if (v) setWallpaper("url", v);
  });
  $("wpfile").addEventListener("change", (e) => {
    const file = e.target.files[0];
    if (!file) return;
    const reader = new FileReader();
    reader.onload = () => setWallpaper("image", reader.result);
    reader.readAsDataURL(file);
  });

  $("freemem").addEventListener("click", async () => {
    await invoke("free_memory");
    toast("Background tabs put to sleep");
    setTimeout(refreshMemory, 800);
  });
  $("clearhist").addEventListener("click", async () => {
    if (!confirm("Clear all browsing history? This can't be undone.")) return;
    await invoke("clear_history");
    toast("History cleared");
  });
  $("newprivate").addEventListener("click", () => invoke("new_incognito_tab"));
  $("clearsite").addEventListener("click", async () => {
    if (!confirm("Clear cookies, caches and site storage for all sites? You'll be signed out everywhere.")) return;
    await invoke("clear_site_data");
    toast("Site data cleared");
  });

  $("uapreset").addEventListener("change", (e) => {
    const v = e.target.value;
    if (v === "custom") {
      $("uarow").hidden = false;
      $("ua").focus();
      return;
    }
    $("uarow").hidden = true;
    settings.user_agent = UA_PRESETS[v] || "";
    $("ua").value = settings.user_agent;
    save();
  });

  // Sidebar scroll-spy.
  const navLinks = [...document.querySelectorAll(".nav a")];
  const spy = new IntersectionObserver(
    (entries) => {
      for (const en of entries) {
        if (en.isIntersecting) {
          const id = en.target.id;
          navLinks.forEach((a) =>
            a.classList.toggle("active", a.getAttribute("href") === `#${id}`)
          );
        }
      }
    },
    { rootMargin: "-20% 0px -70% 0px" }
  );
  document.querySelectorAll(".section").forEach((s) => spy.observe(s));
}

async function init() {
  wireControls();
  // Toggles flipped from the menu (⇧⌘B / ⇧⌘S) while this page is open.
  listen("settings-changed", async () => {
    const s = await invoke("get_settings");
    settings = s;
    $("bmbar").checked = s.show_bookmarks_bar;
    $("verticaltabs").checked = !!s.vertical_tabs;
  });
  const s = await invoke("get_settings");
  settings = s;
  renderTheme();
  renderAccents();
  $("engine").value = s.search_engine;
  $("zoom").value = String(s.default_zoom);
  $("bmbar").checked = s.show_bookmarks_bar;
  $("verticaltabs").checked = !!s.vertical_tabs;
  $("mrutabs").checked = !!s.mru_tab_switching;
  $("discard").value = String(s.discard_after_min);
  $("blockads").checked = !!s.block_ads;
  $("blockmalware").checked = !!s.block_malware;
  $("blockpopups").checked = !!s.block_popups;
  $("blockannoyances").checked = !!s.block_annoyances;
  $("startup").value = s.startup || "home";
  $("devtools").checked = !!s.devtools;
  $("reducetransparency").checked = !!s.reduce_transparency;
  loadUA(s.user_agent || "");
  applyAppearance(settings);
  renderSwatches();
  initTransparencyRow();
  loadBookmarks();
  initBlocker();
  refreshMemory();
  // Cheap (a few syscalls). Background tabs are hidden by the app, so
  // `visibilityState` really is "hidden" there and this stays idle.
  setInterval(() => {
    if (document.visibilityState === "visible") refreshMemory();
  }, 4000);
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") refreshMemory();
  });
}

init();
