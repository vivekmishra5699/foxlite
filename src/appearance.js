// Shared appearance helpers used by every Foxlite surface (chrome + bundled
// pages): resolve the theme and apply the user's accent colour. Centralised so
// "system" theme + custom accent behave identically everywhere.

export function resolveTheme(theme) {
  if (theme === "system") {
    return window.matchMedia("(prefers-color-scheme: dark)").matches
      ? "dark"
      : "light";
  }
  return theme || "dark";
}

// Relative luminance (0..1) of a #rrggbb colour — used to pick readable text on
// top of the accent, so any accent the user picks keeps its labels legible.
function luminance(hex) {
  const m = /^#?([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(hex || "");
  if (!m) return 0;
  const [r, g, b] = [1, 2, 3].map((i) => parseInt(m[i], 16) / 255);
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

// Apply theme + accent from a settings-like object ({ theme, accent }). The
// soft/hover/gradient accent variants derive from `--accent` in theme.css via
// color-mix, so overriding `--accent` here recolours the entire UI.
export function applyAppearance({ theme, accent } = {}) {
  const root = document.documentElement;
  root.dataset.theme = resolveTheme(theme);
  if (accent) {
    root.style.setProperty("--accent", accent);
    root.style.setProperty(
      "--accent-text",
      luminance(accent) > 0.6 ? "#1a1620" : "#ffffff"
    );
  }
}
