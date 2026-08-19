// Address-bar / home-search suggestions, shared by the chrome and the new-tab
// page. Attaches to a text input and shows a dropdown of
//   - the most recent pages when the box is focused with nothing typed, and
//   - matching bookmarks + history (past searches shown as searches) while
//     typing, with a "Search for / Go to <typed text>" row first;
// plus the inline host completion ("git" → "github.com" with the rest
// selected). ↑/↓ move the highlight (filling the box Chrome-style), ↵ opens
// the highlighted row (or the typed text), Esc closes, click picks.
//
// Rendering is plain DOM, rebuilt per result set (≤ `limit` rows). The
// backend does the ranking (`suggestions` command); this file only draws.

import { invoke } from "./ipc.js";
import { GLOBE_SVG, debounce, hostOf, paintFavicon } from "./util.js";

const svg = (body) =>
  `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">${body}</svg>`;
const ICON = {
  search: svg('<circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/>'),
  go: svg('<path d="M5 12h14"/><path d="m13 6 6 6-6 6"/>'),
  clock: svg('<circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/>'),
  star: svg(
    '<polygon points="12 2 15.1 8.3 22 9.3 17 14.1 18.2 21 12 17.8 5.8 21 7 14.1 2 9.3 8.9 8.3"/>'
  ),
};

/// Does the typed text look like an address rather than a search? (Display
/// hint only — the backend decides for real in `url_util::to_url`.)
function looksLikeUrl(text) {
  const s = text.trim();
  if (/\s/.test(s)) return false;
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(s)) return true;
  if (/^localhost(:\d+)?(\/|$)/i.test(s)) return true;
  return /^[^\s/?#]+\.[a-z]{2,}(:\d+)?([/?#]|$)/i.test(s);
}

/// URL without scheme / `www.` / trailing slash, for the dim right-hand text.
function prettyUrl(url) {
  return url
    .replace(/^https?:\/\/(www\.)?/i, "")
    .replace(/\/$/, "")
    .slice(0, 80);
}

/**
 * @param {HTMLInputElement} input
 * @param {object} opts
 * @param {(text: string) => void} opts.onNavigate  open what the user chose
 * @param {(rect: DOMRect | null) => void} [opts.onLayout]  dropdown shown /
 *        moved / hidden (the chrome uses it to grow its webview over the page)
 * @param {number} [opts.limit]      max rows from the backend
 * @param {number} [opts.minWidth]   dropdown is at least this wide
 * @param {boolean} [opts.inline]    inline host completion while typing
 * @param {boolean} [opts.showOnFocus]  open the recent list on focus (off for a
 *        box that is auto-focused on load — it then opens on click / typing)
 */
export function attachSuggest(
  input,
  { onNavigate, onLayout, limit = 8, minWidth = 360, inline = true, showOnFocus = true }
) {
  const panel = document.createElement("div");
  panel.className = "suggest";
  document.body.appendChild(panel);

  let open = false;
  let rows = []; // [{kind, url, title, favicon, query}] — rows[0] is the typed row when text is non-empty
  let sel = -1;
  let typed = ""; // what the user actually typed (restored when arrowing back)
  let original = ""; // box content when it was focused (the current URL)
  let seq = 0; // ignore stale lookups

  function place() {
    const r = input.getBoundingClientRect();
    panel.style.left = `${Math.max(4, r.left)}px`;
    panel.style.top = `${r.bottom + 6}px`;
    panel.style.width = `${Math.max(r.width, minWidth)}px`;
    onLayout?.(panel.getBoundingClientRect());
  }

  function hide() {
    if (!open) return;
    open = false;
    sel = -1;
    rows = [];
    panel.classList.remove("show");
    panel.replaceChildren();
    onLayout?.(null);
  }

  function rowEl(row, i) {
    const el = document.createElement("div");
    el.className = "sg-row" + (i === sel ? " sel" : "");
    el.dataset.i = i;
    const icon = document.createElement("span");
    icon.className = "sg-icon";
    const title = document.createElement("span");
    title.className = "sg-title";
    const sub = document.createElement("span");
    sub.className = "sg-url";
    if (row.kind === "typed") {
      const url = looksLikeUrl(row.query);
      icon.innerHTML = url ? ICON.go : ICON.search;
      title.textContent = row.query;
      sub.textContent = url ? "Go to address" : "Search";
    } else if (row.kind === "search") {
      icon.innerHTML = ICON.search;
      title.textContent = row.query;
      sub.textContent = "Search again";
    } else {
      if (row.favicon) paintFavicon(icon, row.favicon);
      else icon.innerHTML = row.kind === "bookmark" ? ICON.star : GLOBE_SVG;
      title.textContent = row.title || hostOf(row.url);
      sub.textContent = (row.kind === "bookmark" ? "★ " : "") + prettyUrl(row.url);
    }
    el.append(icon, title, sub);
    return el;
  }

  function render() {
    const kids = [];
    if (rows.length && rows[0].kind !== "typed") {
      const head = document.createElement("div");
      head.className = "sg-head";
      head.textContent = "Recent";
      kids.push(head);
    }
    rows.forEach((r, i) => kids.push(rowEl(r, i)));
    panel.replaceChildren(...kids);
    if (!open) {
      open = true;
      panel.classList.add("show");
    }
    place();
  }

  /// Repaint only the highlight (arrow keys / hover).
  function paintSel() {
    for (const el of panel.children) {
      if (el.dataset.i !== undefined) {
        el.classList.toggle("sel", parseInt(el.dataset.i, 10) === sel);
      }
    }
  }

  /// Text a row stands for (what ↵ opens / the box shows when arrowed onto).
  const rowText = (r) => (r.kind === "typed" || r.kind === "search" ? r.query : r.url);

  const lookup = debounce(async (text) => {
    const my = ++seq;
    let list;
    try {
      list = await invoke("suggestions", { query: text, limit });
    } catch {
      list = [];
    }
    if (my !== seq || document.activeElement !== input) return;
    // The box may have changed meanwhile; `typed` tracks the raw text.
    const now = typed.trim();
    if (now !== text.trim()) return;
    rows = now ? [{ kind: "typed", query: now }, ...list] : list;
    if (rows.length === 0) {
      hide();
      return;
    }
    sel = now ? 0 : -1;
    render();
  }, 50);

  function query(text) {
    typed = text;
    lookup(text);
  }

  // Inline completion, debounced so fast typing doesn't queue a lookup per key.
  const complete = debounce(async (text) => {
    const s = await invoke("suggest", { prefix: text });
    if (
      !s ||
      input.value !== text || // user kept typing
      document.activeElement !== input ||
      !s.toLowerCase().startsWith(text.toLowerCase())
    ) {
      return;
    }
    input.value = text + s.slice(text.length);
    input.setSelectionRange(text.length, s.length);
    // The typed row now stands for the completed address.
    if (open && rows[0]?.kind === "typed") {
      rows[0].query = input.value;
      panel.querySelector('.sg-row[data-i="0"]')?.replaceWith(rowEl(rows[0], 0));
    }
  }, 60);

  function pick(row) {
    // The typed row opens whatever the box holds (incl. an inline completion).
    const text = row.kind === "typed" ? input.value : rowText(row);
    hide();
    if (text.trim()) onNavigate(text);
  }

  function move(delta) {
    if (!open || rows.length === 0) return;
    const n = rows.length;
    const hasTyped = rows[0].kind === "typed";
    // In the recent list (nothing typed) the box itself is a position too:
    // ↓ from it goes to the first row, ↑ from the first row back to it.
    if (sel < 0) sel = delta > 0 ? 0 : n - 1;
    else if (sel === 0 && delta < 0 && !hasTyped) sel = -1;
    else sel = (sel + delta + n) % n;
    paintSel();
    const r = rows[sel];
    input.value = sel < 0 ? original : r.kind === "typed" ? typed : rowText(r);
    input.setSelectionRange(input.value.length, input.value.length);
  }

  // ---- wiring ---------------------------------------------------------------
  // Focusing shows recent pages whatever the box holds (the current URL
  // stays selected for overtyping); typing filters.
  input.addEventListener("focus", () => {
    original = input.value;
    if (showOnFocus) query("");
  });
  // Clicking into an already-focused box (re)opens the list.
  input.addEventListener("click", () => {
    if (!open && document.activeElement === input) query(input.value === original ? "" : input.value);
  });
  input.addEventListener("blur", hide);
  input.addEventListener("input", (e) => {
    const text = input.value;
    query(text);
    // Only complete while typing forward at the end of the text.
    if (
      inline &&
      e.inputType?.startsWith("insert") &&
      input.selectionEnd === text.length
    ) {
      complete(text);
    }
  });
  input.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      if (!open) return;
      e.preventDefault();
      move(e.key === "ArrowDown" ? 1 : -1);
    } else if (e.key === "Enter") {
      e.preventDefault();
      e.stopImmediatePropagation();
      if (open && sel >= 0 && rows[sel]) pick(rows[sel]);
      else {
        const text = input.value;
        hide();
        if (text.trim()) onNavigate(text);
      }
    } else if (e.key === "Escape" && open) {
      e.preventDefault();
      e.stopImmediatePropagation();
      hide();
    }
  });
  // Keep focus in the box while clicking a row (no blur → no hide).
  panel.addEventListener("mousedown", (e) => e.preventDefault());
  panel.addEventListener("click", (e) => {
    const el = e.target.closest(".sg-row");
    if (el) pick(rows[parseInt(el.dataset.i, 10)]);
  });
  panel.addEventListener("mouseover", (e) => {
    const el = e.target.closest(".sg-row");
    if (!el) return;
    sel = parseInt(el.dataset.i, 10);
    paintSel();
  });
  window.addEventListener("resize", () => open && place());

  return { hide, isOpen: () => open };
}
