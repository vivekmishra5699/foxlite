// "View Source" page: shows the live DOM serialization captured from the tab
// that was active when ⌥⌘U was pressed. Rendered as plain text with line
// numbers, a line filter, and copy — no highlighting library, to stay small.
// Very large documents render in chunks ("Show more") so the page stays snappy.

import { applyAppearance } from "./appearance.js";
import { invoke } from "./ipc.js";
import { debounce } from "./util.js";

const CHUNK = 3000;

const out = document.getElementById("out");
const urlEl = document.getElementById("url");
let lines = [];
let html = "";
let filter = "";
let rendered = 0; // how many *matching* rows are in the DOM
let cursor = 0; // index into `lines` where the next chunk starts

function lineRow(line, i, f) {
  const row = document.createElement("div");
  row.className = "ln";
  const n = document.createElement("span");
  n.className = "n";
  n.textContent = i + 1;
  const c = document.createElement("span");
  c.className = "c";
  if (f) {
    // Highlight matches without injecting HTML.
    const lower = line.toLowerCase();
    let pos = 0;
    let idx;
    while ((idx = lower.indexOf(f, pos)) !== -1) {
      c.appendChild(document.createTextNode(line.slice(pos, idx)));
      const m = document.createElement("mark");
      m.textContent = line.slice(idx, idx + f.length);
      c.appendChild(m);
      pos = idx + f.length;
    }
    c.appendChild(document.createTextNode(line.slice(pos)));
  } else {
    c.textContent = line;
  }
  row.append(n, c);
  return row;
}

/// Append up to CHUNK matching rows starting at `cursor`.
function renderChunk() {
  const f = filter.toLowerCase();
  const frag = document.createDocumentFragment();
  let added = 0;
  while (cursor < lines.length && added < CHUNK) {
    const line = lines[cursor];
    const i = cursor++;
    if (f && !line.toLowerCase().includes(f)) continue;
    frag.appendChild(lineRow(line, i, f));
    added++;
  }
  rendered += added;
  document.getElementById("more")?.remove();
  if (rendered === 0 && cursor >= lines.length) {
    const e = document.createElement("div");
    e.className = "empty";
    e.textContent = lines.length
      ? "No matching lines."
      : "Nothing captured — use Develop ▸ View Page Source on a page.";
    frag.appendChild(e);
  }
  out.appendChild(frag);
  if (cursor < lines.length) {
    const more = document.createElement("button");
    more.id = "more";
    more.className = "more";
    more.textContent = `Show more (${lines.length - cursor} lines left)`;
    more.addEventListener("click", renderChunk);
    out.appendChild(more);
  }
}

function render(f = "") {
  filter = f;
  rendered = 0;
  cursor = 0;
  out.replaceChildren();
  renderChunk();
}

document.getElementById("filter").addEventListener("input", debounce((e) => render(e.target.value), 100));
document.getElementById("copy").addEventListener("click", async () => {
  try {
    await navigator.clipboard.writeText(html);
  } catch {
    // Clipboard may be unavailable in this context; select all instead.
    const r = document.createRange();
    r.selectNodeContents(out);
    getSelection().removeAllRanges();
    getSelection().addRange(r);
  }
});
let wrap = true;
document.getElementById("wrap").addEventListener("click", (e) => {
  wrap = !wrap;
  e.target.textContent = `Wrap: ${wrap ? "on" : "off"}`;
  // One class on the container instead of touching every row.
  out.classList.toggle("nowrap", !wrap);
});

invoke("get_settings").then(applyAppearance);
invoke("get_view_source").then(([url, src]) => {
  html = src || "";
  urlEl.textContent = url || "";
  document.title = url ? `Source of ${url}` : "View Source";
  lines = html ? html.split("\n") : [];
  render();
});
