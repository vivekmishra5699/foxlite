# Foxlite

A minimal, memory-light browser built on the operating system's WebView via
[Tauri](https://tauri.app) — **WKWebView** on macOS, **WebView2** on Windows.
Using the OS engine means real sites (incl. video/MSE) work, the engine is
shared across apps (low memory), and the binary stays tiny.

> Earlier prototype built on the Servo engine was dropped: Servo has no Media
> Source Extensions / DRM, so streaming video couldn't play. Every shipping
> browser (Zen, Arc, Brave, Orion) is a shell on a production engine — so we are
> too.

## Architecture

```
src/                     chrome UI + bundled internal pages
  index.html/styles.css/chrome.js   tab bar + toolbar + bookmarks/find bars + toasts
  ipc.js                 20-line IPC bridge (invoke/listen/convertFileSrc) on the
                         internals Tauri injects — no `withGlobalTauri` bundle
  util.js                shared helpers (hostOf, wallpaper presets, favicons, debounce)
  appearance.js          shared theme/accent application
  theme.css              shared light/dark design tokens
  pages.css              shared styling for the internal pages
  newtab.html/js         home page: search, wallpaper, clock
  settings.html/js       settings (theme/search/blocker/startup/memory/privacy/developer)
  history.html/js        browsable, searchable history
  source.html/js         "View Page Source" (⌥⌘U) with filter + copy
src-tauri/src/
  main.rs                entry point
  lib.rs                 app setup: window + chrome webview + menu + wiring
  state.rs               BrowserState (tabs, active, chrome height) — pure data
  store.rs               persisted settings+bookmarks (settings.json), session
                         (session.json), history (append-only history.jsonl) and
                         favicon cache (favicons.json); one housekeeping thread
                         with per-file debounce; address-bar suggestion index
  tabs.rs                controller: create/close/select/discard page webviews,
                         downloads, target=_blank, memory report; coalesced
                         `state` pushes (one event per run-loop turn)
  native.rs              macOS hooks on the WKWebView: KVO URL/back-forward
                         observer, exact WebContent pid + footprint, page close /
                         process termination through WebKit, content blocker
                         (cached compile), reduce-transparency preference
                         (no-ops on other platforms)
  blocklist.rs           embedded EasyList/EasyPrivacy rule set (+ tiny fallback)
tools/blocklist/build.mjs  filter lists → WebKit content-blocker JSON (build step)
  menu.rs                native menu bar + accelerators + ☰ popup menu
  commands.rs            IPC commands (thin) called by the chrome + pages
  layout.rs              positions chrome strip + page webviews; background
                         tabs are *hidden* (WebKit throttles them), not parked
  url_util.rs            address-bar text → URL or search; internal-page detection
capabilities/
  default.json           IPC for the chrome webview
  internal-pages.json    IPC for local internal pages only (not external sites)
```

Home, settings, and history are bundled local pages loaded into a tab webview;
the `internal-pages` capability (`local: true`) lets them call into the app
while external `https://` pages in the same webview cannot.

One window hosts a `chrome` webview (our UI) at the top and one `tab-{id}`
webview per tab below it. Rust owns tab state and positions the webviews; the
chrome UI talks to it over IPC and re-renders from `state` events.

## Develop

```bash
npm install            # one-time: fetch the Tauri CLI
npm run dev            # build + run (debug)
npm run build          # package a release app (.dmg/.app on macOS)
```

(Or `cd src-tauri && cargo run` to run without the CLI.)

## How it stays light

- **Ads, trackers, malware and pop-ups never load.** The standard filter
  lists — EasyList, EasyPrivacy, Peter Lowe's, NoCoin, URLhaus + phishing
  host lists and the EasyList Cookie List (the same lists uBlock Origin
  ships) — are converted at build time (`npm run blocklist`, see
  `tools/blocklist/`) into ~180k WebKit content-blocker rules in four
  toggleable categories (ads+trackers, malware/phishing, cookie banners),
  embedded brotli-compressed (~1.7 MB) and compiled once by WebKit (cached
  across launches). Blocked requests are dropped in the network layer, so
  they cost no RAM, CPU, or bandwidth; element-hiding rules remove the empty
  ad slots and consent overlays. On top of that WebKit's own pop-up blocker
  refuses `window.open` without a user gesture, and its Safe Browsing check
  shows a warning before known phishing/malware pages load. This is the same
  mechanism Safari ad blockers and uBO Lite use; extensions themselves can't
  run in WKWebView. All of it is in Settings ▸ Performance.
  (`npm run blocklist:ubo` also pulls uBlock Origin's own GPLv3 lists.)
- **Background tabs are hidden, not just moved off-screen.** WebKit treats a
  hidden view as a real background page: `document.hidden`, throttled timers,
  no rAF, no compositing — the same thing Safari does.
- **Idle tabs sleep.** Background tabs unused for N minutes have their page
  closed and — when no other tab or the chrome shares it — their WebContent
  process terminated through WebKit itself. We read the exact process id from
  the WKWebView to check sharing. Tabs reload on click.
- **Lazy session restore.** Restored tabs are created asleep; only the one you
  look at loads.
- **No polling.** The address bar tracks single-page-app URL changes through a
  native KVO observer instead of running JS in the page on a timer.
- **No extra processes for UI.** The ☰ menu is a native popup, not a webview.
- **Cheap persistence.** History is append-only (`history.jsonl`); settings,
  session and the favicon cache are small files with their own debounce, all
  written by one housekeeping thread — no 1 MB rewrite per navigation. The
  uploaded wallpaper lives in its own file and is served over a custom
  `wallpaper://` scheme (raster types only, sandboxed) rather than being
  JSON-encoded on every save.
- **No third-party favicon service.** Bookmarks and history show the icon the
  site itself reported (cached by host); nothing is sent to a lookup service.
- **UI events are coalesced.** However many navigation/title/favicon changes
  land in one run-loop turn, the chrome gets one `state` event.
- **Reduce transparency** (Settings ▸ Appearance, or the macOS accessibility
  preference) swaps the frosted-glass window for an opaque frame.
- Per-tab memory shows in the tab tooltip and Settings ▸ Performance.

## Engineering guardrails

These came out of the 2026-08 audit and are enforced by CI (`.github/workflows/ci.yml`:
rustfmt, `clippy -D warnings`, unit tests, JS import check, and grep-based tripwires)
plus the scripted self-test (`FOXLITE_SELFTEST=1 target/debug/foxlite`, debug builds only —
opens real sites, closes tabs through the chrome's IPC path, sleeps/revives tabs, checks
the blocker, pop-up blocking, Safe Browsing and private-tab isolation; if a step never
logs `done`, `sample` the process). Please keep them when contributing:

- **Background tabs are hidden, never parked off-screen** (`layout.rs`) — WebKit only
  throttles hidden views.
- **Store writes are split and debounced** (`store.rs`): history is append-only
  `history.jsonl`; settings/session/favicons are small files. Never reintroduce a single
  store rewritten on every navigation, and never serialise while holding the store lock.
- **`state` events are coalesced** (`tabs::emit_current`): one push per run-loop turn.
- **Tab teardown order** (`tabs::teardown_webview`): hide → Tauri `close()` (drops wry's
  delegates and Tauri's reload-on-terminate handler) → *deferred* WebKit terminate/`_close`
  on the retained view. Never terminate a page from inside an IPC/menu callout, and never
  `kill(2)` a pid.
- **Internal pages are detected by app origin** (`url_util::is_internal`), not by path
  suffix.
- **No third-party favicon service; no `withGlobalTauri`; CSP on** — pages use `src/ipc.js`.
- **`wallpaper://` serves raster types only, sandboxed** — custom schemes count as a
  local (IPC-privileged) origin.
- **Anything user-facing that can hang has a watchdog** (`watchdog.rs` writes
  `hang-<ts>.txt` with a stack sample into the app-data folder).
- The blocklist is generated, not hand-edited: `npm run blocklist` → commit
  `src-tauri/blocklist/rules.jsonl.br`.

## Status / roadmap
- [x] Window + chrome + page webviews, address bar, back/forward/reload/stop, search
- [x] Tabs (new/close/switch/reorder/reopen-closed) + keyboard shortcuts via native menu
- [x] Tab titles + real page favicons + loading state; `target=_blank` opens a tab
- [x] Home page (search, wallpaper, clock)
- [x] Settings page (theme, accent, wallpaper, reduce transparency, search
      engine, blocker, startup, memory saver, privacy, developer)
- [x] Bookmarks bar + paged/searchable history page, inline address autocomplete
- [x] Find-in-page (⌘F), zoom (⌘±/⌘0), print (⌘P)
- [x] Private tabs (⇧⌘N): own temporary session, wiped on close, no history,
      links/pop-ups from a private tab stay private, badge in the toolbar
- [x] Downloads to ~/Downloads with a toast + "Show in Finder"
- [x] Memory saver: sleep idle background tabs (+ "sleep now"), memory readout
- [x] Session restore (lazy)
- [x] Ad, tracker, cryptominer, malware/phishing host blocking + cookie-banner
      hiding (EasyList & co. → WebKit content rules), pop-up blocking, Safe
      Browsing warnings
- [x] Developer: Web Inspector (⌥⌘I / Inspect Element), View Source (⌥⌘U),
      Reload Ignoring Cache (⇧⌘R), custom User-Agent, Clear Site Data
- [ ] Profiles (isolated cookies/logins) — deferred
- [ ] Context menus for links (open in new tab), pinned tabs
- [ ] Distribution (sign/notarize macOS; Windows build)
