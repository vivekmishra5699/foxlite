# Foxlite

A minimal, memory-light browser built on the system WebView — **WKWebView** on
macOS — via [Tauri](https://tauri.app). Using the OS engine means real sites,
including streaming video (MSE/DRM), just work, the engine is shared with the
rest of the system, and the app itself is a ~5 MB binary.

> **macOS ships today; Windows and Linux are next.** The build, CI and
> installer currently target macOS, where the browser is being stabilised
> first. Windows (WebView2) and Linux (WebKitGTK) builds are planned in this
> repository — the `#[cfg(not(target_os = "macos"))]` stubs in the Rust code
> are the starting point for them, and are untested until those builds land.

## Features

- **Tabs** — new / close / reorder / reopen-closed, middle-click to close,
  ⌘1…9 to jump, optional **vertical tabs** sidebar (⇧⌘S).
- **⌃⇥ tab switcher** — Arc-style: cards of your most recently used tabs;
  hold ⌃, tap ⇥ / ⇧⇥ to move, release to switch, Esc to cancel. Can be set
  back to in-order cycling.
- **Address bar** — recent pages on click, history / bookmark / past-search
  suggestions while typing, inline address completion; search with
  DuckDuckGo, Google, Bing, Brave, Ecosia, Startpage or Yahoo.
- **Built-in blocking** — EasyList, EasyPrivacy, Peter Lowe's, NoCoin,
  URLhaus, phishing lists and the EasyList Cookie List compiled into ~180k
  WebKit content-blocker rules (ads & trackers, malware/phishing, cookie
  banners — each toggleable), plus WebKit's pop-up blocker and Safe Browsing
  warnings.
- **Memory saver** — background tabs are *hidden* (WebKit throttles them),
  idle ones are put to sleep (page closed, process terminated when nothing
  else shares it) and reload on click; per-tab memory in the tab tooltip and
  Settings ▸ Performance.
- **Private tabs** (⇧⌘N) — own temporary session wiped on close, no history,
  links and pop-ups from a private tab stay private.
- **Home page** with search, clock and wallpaper (presets, URL or upload);
  bookmarks bar; searchable history; find-in-page; zoom; print; downloads to
  ~/Downloads with a "Show in Finder" toast; session restore (lazy — only the
  tab you look at loads).
- **Appearance** — light / dark / system, accent colour, frosted-glass window
  (or "reduce transparency" for an opaque frame).
- **Developer** — Web Inspector (⌥⌘I), View Source (⌥⌘U), reload ignoring
  cache, custom User-Agent, clear site data.

## Install

Download `Foxlite_<version>_aarch64.dmg` from the
[latest release](../../releases/latest) (Apple Silicon, macOS 12+), open it
and drag **Foxlite** to Applications. The app is not notarized yet, so on
first launch right-click ▸ Open (or allow it in System Settings ▸ Privacy &
Security).

### Build from source

```bash
npm install        # one-time: Tauri CLI
npm run dev        # debug build + run
npm run build      # release .app + .dmg in src-tauri/target/release/bundle/
```

Requires a Rust toolchain and Xcode command-line tools. `cd src-tauri &&
cargo run` works without the Tauri CLI.

## Keyboard shortcuts

| Action | Keys |
| --- | --- |
| New tab / private tab / close / reopen | ⌘T · ⇧⌘N · ⌘W · ⇧⌘T |
| Switch tabs (recent first) | ⌃⇥ · ⌃⇧⇥ |
| Jump to tab 1–8 / last | ⌘1…8 · ⌘9 |
| Open location / find in page | ⌘L · ⌘F |
| Back / forward | ⌘[ · ⌘] |
| Reload / ignoring cache / stop | ⌘R · ⇧⌘R · ⌘. |
| Zoom in / out / reset | ⌘+ · ⌘− · ⌘0 |
| Bookmark page / toggle bookmarks bar | ⌘D · ⇧⌘B |
| Toggle vertical tabs | ⇧⌘S |
| History / Settings / Print | ⌘Y · ⌘, · ⌘P |
| Web Inspector / View source | ⌥⌘I · ⌥⌘U |

## Architecture

One window hosts a `chrome` webview (our HTML/CSS/JS UI — tab strip across
the top, or a sidebar down the left with vertical tabs) and one `tab-{id}`
webview per tab beside it. Rust owns tab state and positions the webviews; the
chrome talks to it over IPC and re-renders from `state` events. Home,
settings, history and view-source are bundled local pages loaded into a tab
webview; the `internal-pages` capability (`local: true`) lets them call into
the app while external pages in the same webview cannot.

Two chrome features must paint *over* the page, which a strip-sized webview
can't: the address-bar dropdown and the ⌃⇥ switcher. For those the backend
temporarily grows the chrome webview over the page and raises it above the
tab views (`layout.rs` + `native::bring_to_front`), shrinking it back when the
overlay closes. The switcher's keys come from an app-wide `NSEvent` monitor so
they work while a page has focus.

```
src/                          chrome UI + bundled internal pages
  index.html / styles.css / chrome.js   tab bar, toolbar, bookmarks/find bars, switcher
  suggest.js / suggest.css    address-bar & home-search suggestion dropdown
  newtab, settings, history, source (.html/.js)   internal pages
  ipc.js                      20-line IPC bridge (no `withGlobalTauri` bundle)
  util.js, appearance.js, theme.css, pages.css    shared helpers / tokens
src-tauri/src/
  lib.rs                      app setup: window, chrome webview, menu, key monitor
  state.rs                    BrowserState: tabs, active, MRU order, switcher, overlays
  tabs.rs                     tab controller: create/close/select/sleep, downloads,
                              switcher, coalesced `state` pushes
  layout.rs                   chrome strip/sidebar + page frames, overlays, hidden bg tabs
  native.rs                   WKWebView hooks: KVO URL observer, WebContent pid, page
                              teardown via WebKit, content blocker, key monitor, z-order
  store.rs                    settings.json / session.json / history.jsonl / favicons.json,
                              one debounced housekeeping thread, suggestions
  menu.rs · commands.rs · url_util.rs · blocklist.rs · watchdog.rs · selftest.rs
src-tauri/capabilities/       IPC scopes for the chrome and the internal pages
tools/blocklist/build.mjs     filter lists → WebKit content-blocker rules (npm run blocklist)
```

## How it stays light

- **Blocked requests never leave the network layer** — no RAM, CPU or
  bandwidth; element-hiding rules remove empty ad slots and consent overlays.
  Rules are embedded brotli-compressed (~1.7 MB) and compiled once by WebKit.
- **Background tabs are hidden, not parked off-screen** — WebKit gives them
  `document.hidden`, throttled timers, no rAF and no compositing.
- **Idle tabs sleep** — page closed and WebContent process terminated through
  WebKit (we read the exact pid to check sharing). Restored sessions start
  asleep.
- **No polling** — SPA URL changes arrive through a native KVO observer.
- **No extra UI processes** — the ☰ menu is a native popup; overlays reuse the
  chrome webview.
- **Cheap persistence** — append-only `history.jsonl`; small settings /
  session / favicon files with their own debounce, one housekeeping thread;
  wallpaper images served from disk over a sandboxed `wallpaper://` scheme.
- **No third-party favicon service** — icons come from what sites reported.
- **UI events are coalesced** — one `state` push per run-loop turn.

## Contributing

CI (`.github/workflows/ci.yml`) runs rustfmt, `clippy -D warnings`, unit
tests, a JS syntax/import check and grep-based tripwires for the guardrails
below. Debug builds also ship scripted self-tests — watch the `[foxlite]` log
lines:

```bash
FOXLITE_SELFTEST=1  target/debug/foxlite   # tab lifecycle on real sites, blocker, private tabs
FOXLITE_SELFTEST=ui target/debug/foxlite   # ⌃⇥ switcher, address dropdown + overlay, vertical tabs
```

Guardrails (from the 2026-08 audit — please keep them):

- Background tabs are **hidden**, never parked off-screen (`layout.rs`).
- Store writes stay **split and debounced** (`store.rs`); history is append-only.
- `state` events stay **coalesced** (`tabs::emit_current`).
- Tab teardown order: hide → Tauri `close()` → *deferred* WebKit
  terminate/`_close` on the retained view; never from inside an IPC/menu
  callout, never `kill(2)`.
- Internal pages are detected by **app origin**, not path suffix.
- No third-party favicon service; no `withGlobalTauri`; CSP on — pages use
  `src/ipc.js`.
- `wallpaper://` serves raster types only, sandboxed.
- The blocklist is generated, not hand-edited: `npm run blocklist` → commit
  `src-tauri/blocklist/rules.jsonl.br`.

## Roadmap

- [ ] Sign & notarize the macOS build
- [ ] Link context menu (open in new tab), pinned tabs
- [ ] Profiles (isolated cookies/logins)
- [ ] Windows build (WebView2)
- [ ] Linux build (WebKitGTK)

## License

Foxlite is licensed under the [Apache License 2.0](LICENSE).

The bundled blocklist (`src-tauri/blocklist/rules.jsonl.br`) is compiled from
third-party filter lists — EasyList, EasyPrivacy, Peter Lowe's list, NoCoin,
URLhaus, the malware-filter phishing list and the EasyList Cookie List — which
keep their own licenses and are credited in
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md). That file is regenerated by
`npm run blocklist`; commit it with the rules.

---

Earlier prototype on the Servo engine was dropped: no MSE/DRM meant no
streaming video. Every shipping browser (Zen, Arc, Brave, Orion) is a shell on
a production engine — so is this one.
