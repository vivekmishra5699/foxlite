// Small helpers shared by the chrome and the bundled pages.

import { convertFileSrc } from "./ipc.js";

/// Host part of a URL (or the input itself when it doesn't parse).
export function hostOf(url) {
  try {
    return new URL(url).host || url;
  } catch {
    return url;
  }
}

/// Built-in wallpaper presets (id -> CSS background).
export const PRESETS = {
  aurora: "linear-gradient(135deg,#1e3c72,#2a5298 45%,#6dd5ed)",
  dusk: "linear-gradient(135deg,#355c7d,#6c5b7b 50%,#c06c84)",
  forest: "linear-gradient(135deg,#0f2027,#203a43 50%,#2c5364)",
  ocean: "linear-gradient(135deg,#2193b0,#6dd5ed)",
  sunset: "linear-gradient(135deg,#ff5f6d,#ffc371)",
  mono: "linear-gradient(135deg,#232526,#414345)",
};

/// Image source for a non-preset wallpaper: a remote URL, or the uploaded
/// picture served from disk by the app's `wallpaper://` scheme (the value is a
/// cache-busting version tag).
export function wallpaperSrc(wp) {
  return wp.kind === "image"
    ? `${convertFileSrc("current", "wallpaper")}?v=${encodeURIComponent(wp.value)}`
    : wp.value;
}

/// Full CSS `background` shorthand for a wallpaper setting.
export function wallpaperCss(wp) {
  if (!wp || wp.kind === "preset") return PRESETS[wp?.value] || PRESETS.aurora;
  return `var(--bg-sink) url("${wallpaperSrc(wp).replace(/"/g, '\\"')}") center / cover no-repeat`;
}

/// Run `fn` at most once per `ms` after the last call (trailing edge).
export function debounce(fn, ms) {
  let t;
  return (...args) => {
    clearTimeout(t);
    t = setTimeout(() => fn(...args), ms);
  };
}

/// Neutral "globe" glyph used wherever a favicon is unknown.
export const GLOBE_SVG =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="M3 12h18"/><path d="M12 3a14 14 0 0 1 0 18 14 14 0 0 1 0-18Z"/></svg>';

/// Fill `el` with a favicon <img> (falls back to the globe glyph on error or
/// when there is no icon). Icons come from our own cache (what pages reported)
/// — never from a third-party favicon service.
export function paintFavicon(el, src) {
  if (!src) {
    el.innerHTML = GLOBE_SVG;
    return;
  }
  const img = document.createElement("img");
  img.src = src;
  img.alt = "";
  img.decoding = "async";
  img.loading = "lazy";
  img.addEventListener("error", () => {
    el.innerHTML = GLOBE_SVG;
  });
  el.replaceChildren(img);
}
