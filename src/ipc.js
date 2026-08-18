// Minimal IPC bridge for Foxlite's own pages, built on the internals Tauri
// injects into every webview. This replaces `withGlobalTauri` (which pushed a
// ~40 KB API bundle into every page load of every website and exposed
// `window.__TAURI__` to all sites) with ~20 lines that only our pages import.
//
// Protocol notes: `invoke` and `convertFileSrc` are provided directly by
// `__TAURI_INTERNALS__`; event listeners are registered through the core
// `plugin:event|listen` command with a callback id from `transformCallback`
// (exactly what `@tauri-apps/api/event` does).

const I = window.__TAURI_INTERNALS__;

export function invoke(cmd, args = {}) {
  return I.invoke(cmd, args);
}

export function convertFileSrc(path, protocol = "asset") {
  return I.convertFileSrc(path, protocol);
}

/// Listen for a backend event. Resolves to an unlisten function.
export async function listen(event, handler) {
  const eventId = await I.invoke("plugin:event|listen", {
    event,
    target: { kind: "Any" },
    handler: I.transformCallback(handler),
  });
  return () => I.invoke("plugin:event|unlisten", { event, eventId });
}
