// Home / new-tab page. Runs inside a tab webview that has IPC access (granted
// to local pages by the `internal-pages` capability), so it can ask the backend
// for its wallpaper/theme and drive navigation from the search box.

import { applyAppearance } from "./appearance.js";
import { invoke, listen } from "./ipc.js";
import { PRESETS, wallpaperSrc } from "./util.js";

const incognito = new URLSearchParams(location.search).has("incognito");
if (incognito) document.body.classList.add("incognito");

function applyWallpaper(wp) {
  // Applied to <body> itself (a separate negative-z layer would hide behind the
  // body's own background).
  const b = document.body;
  if (!wp || wp.kind === "preset") {
    b.style.background = PRESETS[wp?.value] || PRESETS.aurora;
  } else {
    b.style.background = "var(--bg-sink)";
    b.style.backgroundImage = `url("${wallpaperSrc(wp)}")`;
    b.style.backgroundSize = "cover";
    b.style.backgroundPosition = "center";
  }
}

async function loadData() {
  const data = await invoke("get_home_data");
  applyAppearance({ theme: data.theme, accent: data.accent });
  applyWallpaper(data.wallpaper);
}

const clockEl = document.getElementById("clock");
const greetingEl = document.getElementById("greeting");
const timeFmt = new Intl.DateTimeFormat([], { hour: "2-digit", minute: "2-digit" });

// The clock shows minutes, so tick once per minute (aligned to the minute
// boundary) rather than every second; skip entirely while hidden.
let clockTimer;
function tickClock() {
  const now = new Date();
  clockEl.textContent = timeFmt.format(now);
  const h = now.getHours();
  const part = h < 5 ? "night" : h < 12 ? "morning" : h < 18 ? "afternoon" : "evening";
  greetingEl.textContent = incognito ? "Private browsing" : `Good ${part}.`;
  clearTimeout(clockTimer);
  if (document.visibilityState === "visible") {
    clockTimer = setTimeout(tickClock, 60_000 - (now.getSeconds() * 1000 + now.getMilliseconds()) + 20);
  }
}
document.addEventListener("visibilitychange", tickClock);

document.getElementById("search").addEventListener("submit", (e) => {
  e.preventDefault();
  const q = document.getElementById("q").value;
  if (q.trim()) invoke("navigate", { input: q });
});

// Re-theme / re-wallpaper live when settings change elsewhere.
listen("settings-changed", loadData);

tickClock();
loadData();
