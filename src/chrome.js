// Chrome UI: renders the tab bar + toolbar + bookmarks/find bars and talks to
// the Rust backend over IPC. State flows one way: backend emits events, we
// re-render. Browser-wide shortcuts live in the native menu (so they work even
// when a web page is focused).
//
// Rendering is incremental: tab elements are keyed by tab id and patched in
// place, toolbar icons are only re-painted when their key changes, and the
// bookmarks bar is only rebuilt when the bookmark list actually differs — so
// the frequent `state` / `chrome-data` events don't rebuild the DOM.

import { applyAppearance } from "./appearance.js";
import { invoke, listen } from "./ipc.js";
import { debounce, hostOf, paintFavicon } from "./util.js";

const tabsEl = document.getElementById("tabs");
const addressEl = document.getElementById("address");
const backEl = document.getElementById("back");
const forwardEl = document.getElementById("forward");
const reloadEl = document.getElementById("reload");
const starEl = document.getElementById("star");
const bookmarksEl = document.getElementById("bookmarks");
const findInput = document.getElementById("find-input");
const menuBtn = document.getElementById("menu");
const toastEl = document.getElementById("toast");
const tabbar = document.getElementById("tabbar");

let current = { tabs: [], active: 0 };
let chromeData = { settings: null, bookmarks: [] };
let findVisible = false;

// Opaque ("reduce transparency") mode and the host OS are decided at launch
// by the backend (`os-macos` keeps room for the traffic lights; other OSes
// have a native title bar above us).
const launchParams = new URLSearchParams(location.search);
if (launchParams.has("opaque")) document.body.classList.add("opaque");
document.body.classList.add(`os-${launchParams.get("os") || "macos"}`);

// ---- SVG icons (crisp line icons; inherit `currentColor`) --------------------
const svg = (body, opts = "") =>
  `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" ${opts}>${body}</svg>`;
const ICONS = {
  back: svg('<path d="m15 18-6-6 6-6"/>'),
  forward: svg('<path d="m9 18 6-6-6-6"/>'),
  reload: svg('<path d="M21 12a9 9 0 1 1-2.64-6.36"/><path d="M21 3v6h-6"/>'),
  plus: svg('<path d="M12 5v14M5 12h14"/>'),
  menu: svg('<path d="M3 6h18M3 12h18M3 18h18"/>'),
  x: svg('<path d="M18 6 6 18M6 6l12 12"/>'),
  up: svg('<path d="m18 15-6-6-6 6"/>'),
  down: svg('<path d="m6 9 6 6 6-6"/>'),
  star: svg(
    '<polygon points="12 2 15.1 8.3 22 9.3 17 14.1 18.2 21 12 17.8 5.8 21 7 14.1 2 9.3 8.9 8.3"/>'
  ),
  starFill: svg(
    '<polygon points="12 2 15.1 8.3 22 9.3 17 14.1 18.2 21 12 17.8 5.8 21 7 14.1 2 9.3 8.9 8.3"/>',
    'fill="currentColor"'
  ),
  home: svg('<path d="m3 9 9-7 9 7v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/><path d="M9 22V12h6v10"/>'),
  gear: svg(
    '<circle cx="12" cy="12" r="3"/><path d="M12 2v3M12 19v3M4.2 4.2l2.1 2.1M17.7 17.7l2.1 2.1M2 12h3M19 12h3M4.2 19.8l2.1-2.1M17.7 6.3l2.1-2.1"/>'
  ),
  clock: svg('<circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/>'),
  incognito: svg(
    '<path d="M2 12h20"/><rect x="3.5" y="10.5" width="6.5" height="6" rx="2"/><rect x="14" y="10.5" width="6.5" height="6" rx="2"/>'
  ),
  sleep: svg('<path d="M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9Z"/>'),
  globe: svg(
    '<circle cx="12" cy="12" r="9"/><path d="M3 12h18"/><path d="M12 3a14 14 0 0 1 0 18 14 14 0 0 1 0-18Z"/>'
  ),
  download: svg('<path d="M12 3v12M6 9l6 6 6-6"/><path d="M4 21h16"/>'),
  code: svg('<path d="m16 18 6-6-6-6M8 6l-6 6 6 6"/>'),
};

// Paint the static toolbar icons (modules run after the DOM is parsed).
backEl.innerHTML = ICONS.back;
forwardEl.innerHTML = ICONS.forward;
document.getElementById("newtab").innerHTML = ICONS.plus;
menuBtn.innerHTML = ICONS.menu;
document.getElementById("find-prev").innerHTML = ICONS.up;
document.getElementById("find-next").innerHTML = ICONS.down;
document.getElementById("find-close").innerHTML = ICONS.x;

/// Set an element's icon only when it changes (avoids re-parsing SVG markup on
/// every state event).
const iconKeys = new WeakMap();
function setIcon(el, key) {
  if (iconKeys.get(el) === key) return;
  iconKeys.set(el, key);
  el.innerHTML = ICONS[key] || "";
}

function activeTab() {
  return current.tabs[current.active];
}

// ---- chrome height -----------------------------------------------------------
// Measure the strips instead of hard-coding their heights: whenever any strip
// resizes or is shown/hidden, report the sum so the backend can position the
// page webview. (The backend ignores reports that don't change the height.)
const strips = ["tabbar", "toolbar", "bookmarks", "findbar"].map((id) =>
  document.getElementById(id)
);
let reportedHeight = 0;
function reportHeight() {
  const height = strips.reduce((sum, el) => sum + el.offsetHeight, 0);
  if (height > 0 && height !== reportedHeight) {
    reportedHeight = height;
    invoke("set_chrome_height", { height });
  }
}
const heightObserver = new ResizeObserver(reportHeight);
for (const el of strips) heightObserver.observe(el);

// ---- internal pages: label + icon ------------------------------------------
const PAGE_META = {
  "settings.html": { label: "Settings", icon: "gear" },
  "history.html": { label: "History", icon: "clock" },
  "source.html": { label: "Source", icon: "code" },
};

// What a tab's icon slot should show, as a cache key + renderer.
function tabIconKey(tab) {
  if (tab.discarded) return "sleep";
  if (tab.loading) return "spin";
  if (tab.favicon) return "img:" + tab.favicon;
  if (tab.url) return "globe";
  if (tab.incognito) return "incognito";
  return PAGE_META[tab.page]?.icon || "home";
}
function paintTabIcon(el, key) {
  el.className = key === "spin" ? "tab-spinner" : "tab-fav";
  if (key === "spin") {
    el.innerHTML = "";
  } else if (key.startsWith("img:")) {
    paintFavicon(el, key.slice(4));
  } else {
    el.innerHTML = ICONS[key] || ICONS.globe;
  }
}

function tabLabel(tab) {
  if (tab.title) return tab.title;
  if (tab.url) return hostOf(tab.url);
  if (tab.incognito) return "Private";
  return PAGE_META[tab.page]?.label || "New Tab";
}

// ---- tabs (incremental, keyed by id) ----------------------------------------
const tabEls = new Map(); // id -> { el, fav, title, iconKey }

function tabIdOf(target) {
  const el = target?.closest?.(".tab");
  return el ? parseInt(el.dataset.id, 10) : null;
}

function createTabEl(id) {
  const el = document.createElement("div");
  el.className = "tab";
  el.draggable = true;
  el.dataset.id = id;
  const fav = document.createElement("span");
  fav.className = "tab-fav";
  const title = document.createElement("span");
  title.className = "tab-title";
  const close = document.createElement("button");
  close.className = "tab-close";
  close.innerHTML = ICONS.x;
  close.title = "Close tab (⌘W)";
  el.append(fav, title, close);
  return { el, fav, title, iconKey: "" };
}

// One set of delegated listeners on the tab strip instead of ~8 closures per
// tab: click/select, middle-click/close, close button, hover tooltip, drag to
// reorder.
tabsEl.addEventListener("click", (e) => {
  const id = tabIdOf(e.target);
  if (id === null) return;
  if (e.target.closest(".tab-close")) {
    e.stopPropagation();
    invoke("close_tab", { id });
  } else {
    invoke("select_tab", { id });
  }
});
tabsEl.addEventListener("auxclick", (e) => {
  const id = tabIdOf(e.target);
  if (id !== null && e.button === 1) {
    e.preventDefault();
    invoke("close_tab", { id });
  }
});
tabsEl.addEventListener("mouseover", (e) => {
  const tabEl = e.target.closest(".tab");
  if (tabEl && !tabEl.contains(e.relatedTarget)) {
    memoryTooltip(tabEl, parseInt(tabEl.dataset.id, 10));
  }
});
tabsEl.addEventListener("dragstart", (e) => {
  const tabEl = e.target.closest(".tab");
  if (!tabEl) return;
  e.dataTransfer.setData("text/x-foxlite-tab", tabEl.dataset.id);
  e.dataTransfer.effectAllowed = "move";
  tabEl.classList.add("dragging");
});
tabsEl.addEventListener("dragend", (e) => {
  e.target.closest?.(".tab")?.classList.remove("dragging");
  clearDropMarks();
});
tabsEl.addEventListener("dragover", (e) => {
  const tabEl = e.target.closest(".tab");
  if (!tabEl || !e.dataTransfer.types.includes("text/x-foxlite-tab")) return;
  e.preventDefault();
  e.stopPropagation();
  e.dataTransfer.dropEffect = "move";
  const before = e.offsetX < tabEl.offsetWidth / 2;
  clearDropMarks();
  tabEl.classList.add(before ? "drop-before" : "drop-after");
});
tabsEl.addEventListener("drop", (e) => {
  const tabEl = e.target.closest(".tab");
  if (!tabEl || !e.dataTransfer.types.includes("text/x-foxlite-tab")) return;
  e.preventDefault();
  e.stopPropagation();
  clearDropMarks();
  const dragged = parseInt(e.dataTransfer.getData("text/x-foxlite-tab"), 10);
  const targetId = parseInt(tabEl.dataset.id, 10);
  const from = current.tabs.findIndex((t) => t.id === dragged);
  const targetIdx = current.tabs.findIndex((t) => t.id === targetId);
  if (from < 0 || targetIdx < 0 || dragged === targetId) return;
  const before = e.offsetX < tabEl.offsetWidth / 2;
  let to = before ? targetIdx : targetIdx + 1;
  if (from < to) to -= 1; // account for removal shifting indices
  invoke("move_tab", { id: dragged, to });
});

function clearDropMarks() {
  for (const { el } of tabEls.values()) el.classList.remove("drop-before", "drop-after");
}

function updateTabEl(rec, tab, active) {
  const { el, fav, title } = rec;
  el.classList.toggle("active", active);
  el.classList.toggle("discarded", !!tab.discarded);
  el.classList.toggle("incognito", !!tab.incognito);
  const label = tabLabel(tab);
  if (title.textContent !== label) title.textContent = label;
  const tip = tab.discarded
    ? `${tab.url || label} — asleep (click to reload)`
    : tab.url || label;
  if (el.dataset.tip !== tip) {
    el.dataset.tip = tip;
    el.title = tip;
  }
  const key = tabIconKey(tab);
  if (rec.iconKey !== key) {
    rec.iconKey = key;
    paintTabIcon(fav, key);
  }
}

function renderTabs() {
  const seen = new Set();
  const order = [];
  current.tabs.forEach((tab, i) => {
    let rec = tabEls.get(tab.id);
    if (!rec) {
      rec = createTabEl(tab.id);
      tabEls.set(tab.id, rec);
    }
    updateTabEl(rec, tab, i === current.active);
    seen.add(tab.id);
    order.push(rec.el);
  });
  for (const [id, rec] of tabEls) {
    if (!seen.has(id)) {
      rec.el.remove();
      tabEls.delete(id);
    }
  }
  // Only touch the DOM order when it actually changed (moving nodes keeps
  // their <img> loaded, but avoiding it entirely is cheaper still).
  const kids = tabsEl.children;
  let same = kids.length === order.length;
  for (let i = 0; same && i < order.length; i++) same = kids[i] === order[i];
  if (!same) tabsEl.replaceChildren(...order);
  // Keep the active tab in view when many are open.
  const act = tabEls.get(activeTab()?.id);
  if (act && !same) act.el.scrollIntoView({ inline: "nearest", block: "nearest" });
}

// Memory tooltip: cached for a few seconds so hovering across tabs is cheap.
let memCache = { at: 0, tabs: [] };
async function memoryTooltip(el, id) {
  if (Date.now() - memCache.at > 3000) {
    try {
      const r = await invoke("memory_report");
      memCache = { at: Date.now(), tabs: r.available ? r.tabs : [] };
    } catch {
      memCache = { at: Date.now(), tabs: [] };
    }
  }
  const m = memCache.tabs.find((t) => t.id === id);
  const base = el.dataset.tip || "";
  el.title = m ? `${base}\n${Math.round(m.bytes / 1048576)} MB` : base;
}

// ---- address bar + toolbar ---------------------------------------------------
function renderAddressAndStar() {
  const active = activeTab();
  document.body.classList.toggle("private", !!active?.incognito);
  if (active && document.activeElement !== addressEl) {
    addressEl.value = active.url || "";
  }
  const url = active?.url || "";
  const bookmarked = !!url && chromeData.bookmarks.some((b) => b.url === url);
  setIcon(starEl, bookmarked ? "starFill" : "star");
  starEl.classList.toggle("on", bookmarked);
  starEl.disabled = !url;

  backEl.disabled = !active?.can_back;
  forwardEl.disabled = !active?.can_forward;

  const loading = !!active?.loading;
  setIcon(reloadEl, loading ? "x" : "reload");
  const tip = loading ? "Stop (⌘.)" : "Reload (⌘R)";
  if (reloadEl.title !== tip) reloadEl.title = tip;
  reloadEl.dataset.loading = loading ? "1" : "";
}

// ---- bookmarks bar -----------------------------------------------------------
let bookmarksSig = "";
function renderBookmarks() {
  const list = chromeData.bookmarks || [];
  // Rebuild only when the list really changed (any settings change re-sends it).
  const sig = list.map((b) => `${b.url}${b.title}${b.favicon}`).join("");
  if (sig === bookmarksSig) return;
  bookmarksSig = sig;
  if (list.length === 0) {
    const hint = document.createElement("span");
    hint.className = "bm-empty";
    hint.textContent = "Bookmarks you add will show up here.";
    bookmarksEl.replaceChildren(hint);
    return;
  }
  bookmarksEl.replaceChildren(
    ...list.map((b) => {
      const el = document.createElement("div");
      el.className = "bm";
      el.title = b.url;
      el.dataset.url = b.url;
      const fav = document.createElement("span");
      fav.className = "tab-fav";
      paintFavicon(fav, b.favicon);
      const label = document.createElement("span");
      label.textContent = b.title || hostOf(b.url);
      el.append(fav, label);
      return el;
    })
  );
}
bookmarksEl.addEventListener("click", (e) => {
  const url = e.target.closest(".bm")?.dataset.url;
  if (url) invoke("navigate", { input: url });
});
bookmarksEl.addEventListener("auxclick", (e) => {
  const url = e.target.closest(".bm")?.dataset.url;
  if (url && e.button === 1) invoke("open_url_new_tab", { url });
});

function applyChromeData() {
  if (chromeData.settings) {
    applyAppearance(chromeData.settings);
    document.body.classList.toggle(
      "show-bookmarks",
      !!chromeData.settings.show_bookmarks_bar
    );
  }
  renderBookmarks();
  renderAddressAndStar();
}

// Keep the window frame colour in sync when the OS flips light/dark while the
// theme setting is "system".
window
  .matchMedia("(prefers-color-scheme: dark)")
  .addEventListener("change", () => {
    if (chromeData.settings?.theme === "system") applyAppearance(chromeData.settings);
  });

// ---- find bar ----------------------------------------------------------------
function toggleFind(show) {
  findVisible = show;
  document.body.classList.toggle("show-find", show);
  if (show) {
    findInput.focus();
    findInput.select();
  } else {
    findInput.value = "";
    invoke("find_clear");
  }
}

function runFind(forward) {
  const query = findInput.value;
  if (query) invoke("find_in_page", { query, forward });
}

// ---- toast (downloads etc.) --------------------------------------------------
let toastTimer;
function showToast(text, { action, onAction, sticky } = {}) {
  toastEl.replaceChildren();
  const icon = document.createElement("span");
  icon.className = "toast-icon";
  icon.innerHTML = ICONS.download;
  const msg = document.createElement("span");
  msg.className = "toast-msg";
  msg.textContent = text;
  toastEl.append(icon, msg);
  if (action) {
    const btn = document.createElement("button");
    btn.textContent = action;
    btn.addEventListener("click", () => {
      onAction?.();
      hideToast();
    });
    toastEl.appendChild(btn);
  }
  const close = document.createElement("button");
  close.className = "toast-x";
  close.innerHTML = ICONS.x;
  close.addEventListener("click", hideToast);
  toastEl.appendChild(close);
  toastEl.classList.add("show");
  clearTimeout(toastTimer);
  if (!sticky) toastTimer = setTimeout(hideToast, 6000);
}
function hideToast() {
  toastEl.classList.remove("show");
}

// ---- events ------------------------------------------------------------------
function focusAddress() {
  addressEl.focus();
  addressEl.select();
}

async function init() {
  await Promise.all([
    listen("state", (event) => {
      current = event.payload;
      renderTabs();
      renderAddressAndStar();
    }),
    listen("chrome-data", (event) => {
      chromeData = event.payload;
      applyChromeData();
    }),
    listen("menu-action", (event) => {
      if (event.payload === "find") toggleFind(true);
      else if (event.payload === "focus-address") focusAddress();
    }),
    // Debug self-test relay: the backend can ask the chrome to issue a command
    // over the real IPC path (only the backend can emit to this webview).
    listen("selftest", (event) => {
      const { cmd, args } = event.payload || {};
      if (cmd) invoke(cmd, args || {});
    }),
    listen("download", (event) => {
      const { status, name, path } = event.payload;
      if (status === "started") {
        showToast(`Downloading ${name}…`, { sticky: true });
      } else if (status === "done") {
        showToast(`Downloaded ${name}`, {
          action: path ? "Show" : undefined,
          onAction: () => invoke("reveal_path", { path }),
        });
      } else {
        showToast(`Download failed: ${name}`);
      }
    }),
  ]);
  invoke("request_state");
  reportHeight();
}
init();

// ---- toolbar wiring ----------------------------------------------------------
document.getElementById("newtab").addEventListener("click", () => invoke("new_tab"));
backEl.addEventListener("click", () => invoke("go_back"));
forwardEl.addEventListener("click", () => invoke("go_forward"));
reloadEl.addEventListener("click", () =>
  invoke(reloadEl.dataset.loading ? "stop_loading" : "reload")
);
starEl.addEventListener("click", () => invoke("bookmark_current"));

// Hamburger menu: a native popup anchored under the button (no extra webview).
menuBtn.addEventListener("click", () => {
  const r = menuBtn.getBoundingClientRect();
  // Right-aligned under the button so it never spills past the window edge.
  invoke("show_menu", { right: r.right, y: r.bottom + 2 });
});

// Drag a link from a page onto the chrome to open it in a new tab. We
// preventDefault across the WHOLE window (capture phase) so the chrome webview
// itself never navigates to the dropped URL — that would replace our UI with
// the page. Dropping anywhere on the chrome opens a new tab. (Tab-reorder drags
// are handled by the tab strip and skipped here.)
function isTabDrag(dt) {
  return dt?.types?.includes("text/x-foxlite-tab");
}
function urlFromDrop(dt) {
  const raw =
    dt.getData("text/uri-list") || dt.getData("text/plain") || dt.getData("URL");
  // uri-list can hold comment lines (#...) and multiple entries.
  return (raw || "")
    .split(/\r?\n/)
    .map((s) => s.trim())
    .find((s) => s && !s.startsWith("#"));
}
window.addEventListener(
  "dragover",
  (e) => {
    e.preventDefault();
    if (isTabDrag(e.dataTransfer)) return;
    e.dataTransfer.dropEffect = "copy";
    tabbar.classList.add("drop-target");
  },
  true
);
window.addEventListener("dragend", () => tabbar.classList.remove("drop-target"));
window.addEventListener("dragleave", (e) => {
  if (!e.relatedTarget) tabbar.classList.remove("drop-target");
});
window.addEventListener(
  "drop",
  (e) => {
    e.preventDefault();
    tabbar.classList.remove("drop-target");
    if (isTabDrag(e.dataTransfer)) return;
    const url = urlFromDrop(e.dataTransfer);
    if (url) invoke("open_url_new_tab", { url });
  },
  true
);

// ---- address bar: enter / escape / inline autocomplete ----------------------
addressEl.addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    invoke("navigate", { input: addressEl.value });
    addressEl.blur();
  } else if (e.key === "Escape") {
    addressEl.value = activeTab()?.url || "";
    addressEl.blur();
  }
});
// Select-all on focus (click or ⌘L), like every browser.
addressEl.addEventListener("focus", () => {
  requestAnimationFrame(() => addressEl.select());
});
// Inline completion, debounced so fast typing doesn't queue a lookup per key.
const suggestFor = debounce(async (typed) => {
  const s = await invoke("suggest", { prefix: typed });
  if (
    !s ||
    addressEl.value !== typed || // user kept typing
    document.activeElement !== addressEl ||
    !s.toLowerCase().startsWith(typed.toLowerCase())
  ) {
    return;
  }
  addressEl.value = typed + s.slice(typed.length);
  addressEl.setSelectionRange(typed.length, s.length);
}, 60);
addressEl.addEventListener("input", (e) => {
  // Only complete while typing forward at the end of the text.
  if (
    !e.inputType?.startsWith("insert") ||
    addressEl.selectionEnd !== addressEl.value.length
  ) {
    return;
  }
  suggestFor(addressEl.value);
});

// ---- find bar wiring ---------------------------------------------------------
findInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    e.preventDefault();
    runFind(!e.shiftKey);
  } else if (e.key === "Escape") {
    toggleFind(false);
  }
});
document.getElementById("find-next").addEventListener("click", () => runFind(true));
document.getElementById("find-prev").addEventListener("click", () => runFind(false));
document
  .getElementById("find-close")
  .addEventListener("click", () => toggleFind(false));

// ---- shortcuts handled in the chrome webview ---------------------------------
// (Everything else is in the native menu so it works regardless of which
// webview is focused.)
window.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && findVisible && document.activeElement !== findInput) {
    toggleFind(false);
  }
});
