// History page. Lists visits newest-first, grouped by day, with search + clear.
//
// The backend pages the list (`get_history(offset, limit, query)`), so opening
// this page never serialises or renders thousands of rows: the first page
// renders immediately and more rows stream in as you scroll. Favicons come
// from the app's own cache (what the sites themselves reported).

import { applyAppearance } from "./appearance.js";
import { invoke } from "./ipc.js";
import { debounce, hostOf, paintFavicon } from "./util.js";

const PAGE = 150;

const listEl = document.getElementById("list");
const searchEl = document.getElementById("search");

// One formatter each, built once (constructing Intl objects per row is slow).
const timeFmt = new Intl.DateTimeFormat([], { hour: "2-digit", minute: "2-digit" });
const dayFmt = new Intl.DateTimeFormat([], { weekday: "long", month: "long", day: "numeric" });

/// Local calendar day index for an epoch-seconds timestamp.
function dayIndex(ts) {
  const d = new Date(ts * 1000);
  return Math.floor((ts - d.getTimezoneOffset() * 60) / 86400);
}
const todayIdx = dayIndex(Date.now() / 1000);
function dayLabel(ts) {
  const idx = dayIndex(ts);
  if (idx === todayIdx) return "Today";
  if (idx === todayIdx - 1) return "Yesterday";
  return dayFmt.format(new Date(ts * 1000));
}

// Current listing: query, how many rows are shown, whether more exist.
let query = "";
let shown = 0;
let total = 0;
let lastDay = null;
let loading = false;
let sentinel = null;

function row(e, favicons) {
  const host = hostOf(e.url);
  const a = document.createElement("a");
  a.className = "item";
  a.href = e.url;
  a.dataset.url = e.url;

  const fav = document.createElement("span");
  fav.className = "fav";
  paintFavicon(fav, favicons[host.split(":")[0]]);

  const t = document.createElement("span");
  t.className = "t";
  t.textContent = e.title?.trim() || host;

  const u = document.createElement("span");
  u.className = "u";
  u.textContent = host;

  const time = document.createElement("span");
  time.className = "time";
  time.textContent = timeFmt.format(new Date(e.ts * 1000));

  a.append(fav, t, u, time);
  return a;
}

// Navigation is delegated: one listener for every row.
listEl.addEventListener("click", (ev) => {
  const url = ev.target.closest(".item")?.dataset.url;
  if (!url) return;
  ev.preventDefault();
  invoke("navigate", { input: url });
});

const observer = new IntersectionObserver((entries) => {
  if (entries.some((en) => en.isIntersecting)) loadMore();
});

/// Fetch and append the next page for the current query.
async function loadMore() {
  if (loading || (shown > 0 && shown >= total)) return;
  loading = true;
  const q = query;
  const page = await invoke("get_history", { offset: shown, limit: PAGE, query: q });
  if (q !== query) {
    // A newer search superseded this request: drop it and load for the new one.
    loading = false;
    loadMore();
    return;
  }
  total = page.total;
  const frag = document.createDocumentFragment();
  for (const e of page.entries) {
    const label = dayLabel(e.ts);
    if (label !== lastDay) {
      lastDay = label;
      const h = document.createElement("div");
      h.className = "day";
      h.textContent = label;
      frag.appendChild(h);
    }
    frag.appendChild(row(e, page.favicons));
  }
  shown += page.entries.length;
  if (sentinel) sentinel.remove();
  if (shown === 0) {
    const empty = document.createElement("div");
    empty.className = "empty";
    empty.textContent = q ? "No matching history." : "No history yet.";
    frag.appendChild(empty);
  }
  listEl.appendChild(frag);
  if (shown < total) {
    // Sentinel just below the last row: scrolling to it loads the next page.
    sentinel = document.createElement("div");
    sentinel.className = "more";
    listEl.appendChild(sentinel);
    observer.observe(sentinel);
  }
  loading = false;
}

/// Start over with a new query.
function reset(q = "") {
  query = q;
  shown = 0;
  total = 0;
  lastDay = null;
  if (sentinel) observer.unobserve(sentinel);
  sentinel = null;
  listEl.replaceChildren();
  loadMore();
}

searchEl.addEventListener("input", debounce((e) => reset(e.target.value.trim()), 120));

document.getElementById("clear").addEventListener("click", async () => {
  await invoke("clear_history");
  reset(query);
});

invoke("get_settings").then(applyAppearance);
reset();
