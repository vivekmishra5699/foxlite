//! Tab controller: creates/destroys/positions the per-tab page webviews and
//! keeps the chrome UI in sync via `state` events.
//!
//! Each public function takes only the `AppHandle` and locks the shared state
//! internally for short critical sections — it never holds a lock across
//! webview creation, a native call, or an `emit`, so there's no risk of
//! re-entrant deadlock (native callbacks such as KVO fire on the main thread).
//!
//! UI pushes are coalesced: `emit_current` marks the state dirty and one
//! `state` event goes out on the next main-loop turn, however many changes
//! (KVO URL/back/forward, title, load start/finish, favicon…) landed in the
//! meantime.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tauri::{
    webview::{DownloadEvent, NewWindowResponse, PageLoadEvent, WebviewBuilder},
    AppHandle, Emitter, LogicalPosition, Manager, WebviewUrl,
};
use url::Url;

use crate::blocklist;
use crate::layout;
use crate::menu;
use crate::native::{self, NavState};
use crate::state::{self, label_for, Tab, SOURCE_CAP};
use crate::store::{self, BookmarkView, Dirty, SessionTab, Settings};
use crate::url_util;

/// Under-page tint for transparent (frosted) webviews: what WebKit paints where
/// the page hasn't yet, e.g. freshly exposed area on resize. Translucent frame
/// colour instead of the default black.
pub const FRAME_TINT: tauri::utils::config::Color = tauri::utils::config::Color(22, 21, 25, 110);

/// Opaque frame colours used when transparency is reduced (dark / light).
pub const FRAME_DARK: tauri::utils::config::Color = tauri::utils::config::Color(22, 21, 25, 255);
pub const FRAME_LIGHT: tauri::utils::config::Color =
    tauri::utils::config::Color(236, 235, 241, 255);

/// Solid frame colour for the current theme setting.
pub fn frame_color(dark: bool) -> tauri::utils::config::Color {
    if dark {
        FRAME_DARK
    } else {
        FRAME_LIGHT
    }
}

/// Largest `data:` favicon we keep per tab (bigger ones are ignored — they
/// would be cloned into every `state` event).
const FAVICON_DATA_CAP: usize = 8 * 1024;

/// Snapshot pushed to the chrome UI on every tab change.
#[derive(serde::Serialize, Clone)]
struct UiState {
    tabs: Vec<Tab>,
    active: usize,
}

/// Settings + bookmarks pushed to the chrome UI (drives theme + bookmarks bar).
#[derive(serde::Serialize, Clone)]
struct ChromeData {
    settings: Settings,
    bookmarks: Vec<BookmarkView>,
}

/// Download progress notification for the chrome's toast.
#[derive(serde::Serialize, Clone)]
struct DownloadNotice {
    status: &'static str, // "started" | "done" | "failed"
    name: String,
    path: String,
}

// ---- deferral / coalescing ---------------------------------------------------

/// Run `f` on the main thread on a *later* run-loop turn (never synchronously),
/// e.g. to hop off a WebKit delegate callout. Uses Tauri's existing async
/// runtime instead of spawning an OS thread.
pub fn defer_main(app: &AppHandle, f: impl FnOnce(&AppHandle) + Send + 'static) {
    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        let inner = h.clone();
        let _ = h.run_on_main_thread(move || f(&inner));
    });
}

static EMIT_PENDING: AtomicBool = AtomicBool::new(false);

/// Schedule a `state` push to the chrome UI (coalesced: many calls in one
/// run-loop turn produce one event). Also keeps the persisted session in step.
pub fn emit_current(app: &AppHandle) {
    if EMIT_PENDING.swap(true, Ordering::AcqRel) {
        return;
    }
    defer_main(app, |app| {
        EMIT_PENDING.store(false, Ordering::Release);
        emit_now(app);
    });
}

/// Push the current tab list to the chrome UI right now.
pub fn emit_now(app: &AppHandle) {
    let ui = {
        let state = state::lock(app);
        UiState {
            tabs: state.tabs.clone(),
            active: state.active,
        }
    };
    let _ = app.emit_to("chrome", "state", ui);
    sync_session(app);
}

/// Keep the persisted session (for "restore tabs on startup") equal to the
/// open non-private tabs. Compares in place; only allocates when it changed.
fn sync_session(app: &AppHandle) {
    let state = state::lock(app);
    let mut store = store::lock(app);
    let live = state.tabs.iter().filter(|t| !t.incognito);
    let mut active = 0;
    let mut same = true;
    let mut n = 0;
    for (i, t) in state.tabs.iter().enumerate() {
        if t.incognito {
            continue;
        }
        if i == state.active {
            active = n;
        }
        match store.session.get(n) {
            Some(s) if s.url == t.url && s.title == t.title && s.page == t.page => {}
            _ => same = false,
        }
        n += 1;
    }
    same &= n == store.session.len() && store.session_active == active;
    if same {
        return;
    }
    store.session = live
        .map(|t| SessionTab {
            url: t.url.clone(),
            title: t.title.clone(),
            page: t.page.clone(),
        })
        .collect();
    store.session_active = active;
    drop(store);
    drop(state);
    store::touch(app, Dirty::Session);
}

/// Push settings + bookmarks to the chrome UI.
pub fn emit_chrome_data(app: &AppHandle) {
    let data = {
        let s = store::lock(app);
        ChromeData {
            settings: s.settings.clone(),
            bookmarks: s.bookmarks_view(),
        }
    };
    let _ = app.emit_to("chrome", "chrome-data", data);
}

// ---- tab records ------------------------------------------------------------

fn new_tab_record(default_zoom: f64, id: usize, url: String, page: String, incognito: bool) -> Tab {
    Tab {
        id,
        url,
        title: String::new(),
        loading: false,
        incognito,
        zoom: default_zoom,
        discarded: false,
        page,
        favicon: String::new(),
        can_back: false,
        can_forward: false,
        last_active: Some(Instant::now()),
    }
}

/// Register a new tab in state and make it active; returns its id.
fn register_tab(app: &AppHandle, url: String, page: String, incognito: bool) -> usize {
    let default_zoom = store::lock(app).settings.default_zoom;
    // A tab opened straight on a URL (link in new tab, bookmark middle-click,
    // reopen, dropped link) is a visit too — `apply_url` only records URL
    // *changes*, so record the starting page here.
    if !url.is_empty() && !incognito {
        store::lock(app).record_visit(&url, "");
        store::touch(app, Dirty::History);
    }
    let mut state = state::lock(app);
    state.deactivate_current(); // the tab being backgrounded starts its idle clock
    let id = state.allocate_id();
    state
        .tabs
        .push(new_tab_record(default_zoom, id, url, page, incognito));
    state.active = state.tabs.len() - 1;
    state.note_active();
    id
}

/// Drop a tab record whose webview could not be built, and re-select a
/// sensible neighbour.
fn unregister_failed(app: &AppHandle, id: usize) {
    crate::dbg_log!("could not create webview for tab {id}");
    let now_empty = {
        let mut state = state::lock(app);
        state.remove(id);
        state.tabs.is_empty()
    };
    if now_empty {
        // Nothing left to show; try the home page once more (if that also
        // fails we still emit so the chrome shows an empty tab bar).
        let id = register_tab(app, String::new(), url_util::NEW_TAB_PAGE.into(), false);
        if !build_tab(app, id) {
            state::lock(app).remove(id);
        }
    }
    layout::relayout(app);
    emit_current(app);
}

/// The webview target a tab should (re)load: its external URL, or the bundled
/// internal page it was showing.
fn target_for(url: &str, page: &str, incognito: bool) -> WebviewUrl {
    if url.is_empty() {
        let page = if page.is_empty() {
            url_util::NEW_TAB_PAGE
        } else {
            page
        };
        let path = if incognito && page == url_util::NEW_TAB_PAGE {
            format!("{page}?incognito=1")
        } else {
            page.to_string()
        };
        return WebviewUrl::App(path.into());
    }
    match Url::parse(url) {
        Ok(u) => WebviewUrl::External(u),
        Err(_) => WebviewUrl::App(url_util::NEW_TAB_PAGE.into()),
    }
}

/// Internal pages other than the home page render frosted (transparent
/// webview) to match the chrome; the wallpapered home page and web content
/// stay opaque.
fn is_frosted_page(url: &str, page: &str) -> bool {
    url.is_empty() && !page.is_empty() && page != url_util::NEW_TAB_PAGE
}

// ---- webview construction ---------------------------------------------------

/// Turn a wry download event into the chrome's toast notice.
fn download_notice(event: DownloadEvent<'_>) -> Option<DownloadNotice> {
    match event {
        DownloadEvent::Requested { url, destination } => Some(DownloadNotice {
            status: "started",
            name: destination
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| url.to_string()),
            path: destination.to_string_lossy().to_string(),
        }),
        DownloadEvent::Finished { url, path, success } => Some(DownloadNotice {
            status: if success { "done" } else { "failed" },
            name: url
                .path_segments()
                .and_then(|mut s| s.next_back())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| url.to_string()),
            path: path
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
        }),
        _ => None,
    }
}

/// Build the webview for a registered tab (reading its URL/page/private flag
/// from state), wiring up navigation / title / loading / new-window / download
/// callbacks, and position it. Returns `false` if the webview could not be
/// created (the tab record is left to the caller to roll back).
fn build_tab(app: &AppHandle, id: usize) -> bool {
    let Some(window) = app.get_window("main") else {
        return false;
    };
    let Some((w, h)) = layout::window_size(&window) else {
        return false;
    };
    let (pos, size, target, incognito, transparent) = {
        let state = state::lock(app);
        let Some(tab) = state.tab(id) else {
            return false;
        };
        let (_, pos, size) = layout::frames(&state, w, h);
        (
            pos,
            size,
            target_for(&tab.url, &tab.page, tab.incognito),
            tab.incognito,
            is_frosted_page(&tab.url, &tab.page),
        )
    };
    let (devtools, user_agent, reduce_transparency, dark) = {
        let s = store::lock(app);
        (
            s.settings.devtools,
            s.settings.user_agent.clone(),
            !cfg!(target_os = "macos")
                || s.settings.reduce_transparency
                || native::system_reduce_transparency(),
            s.settings.theme != "light",
        )
    };
    let label = label_for(id);

    let title_app = app.clone();
    let load_app = app.clone();
    let nw_app = app.clone();
    let dl_app = app.clone();

    // NB: no `on_navigation` hook — wry fires it for *every* frame (ad/embed
    // iframes included), so it can't tell us the page URL. Main-frame URL
    // changes arrive via `on_page_load` (start/finish) and the native KVO
    // observer, both of which are main-frame only.
    let mut builder = WebviewBuilder::new(&label, target)
        .on_document_title_changed(move |_webview, title| on_title(&title_app, id, title))
        .on_page_load(move |_webview, payload| {
            let finished = matches!(payload.event(), PageLoadEvent::Finished);
            on_page_loaded(&load_app, id, finished, payload.url().to_string());
        })
        // `target="_blank"` links and `window.open` open a new tab (never a
        // new OS window). Deny the popup; the tab loads the URL on its own —
        // on the next run-loop turn, not from inside WebKit's callout.
        .on_new_window(move |url, _features| {
            if matches!(url.scheme(), "http" | "https") {
                // A private tab's pop-ups/links stay private.
                defer_main(&nw_app, move |app| open_external_as(app, url, incognito));
            }
            NewWindowResponse::Deny
        })
        // Downloads go to ~/Downloads (wry picks a non-clobbering name); the
        // chrome shows a toast on start/finish.
        .on_download(move |_webview, event| {
            if let Some(notice) = download_notice(event) {
                let _ = dl_app.emit_to("chrome", "download", notice);
            }
            true
        })
        .devtools(devtools);
    if incognito {
        builder = builder.incognito(true);
    }
    if transparent {
        // Frosted internal page: same translucent under-page tint as the chrome
        // so exposed areas never flash black — or the solid frame colour when
        // transparency is reduced.
        builder = if reduce_transparency {
            builder.background_color(frame_color(dark))
        } else {
            builder.transparent(true).background_color(FRAME_TINT)
        };
    }
    if !user_agent.is_empty() {
        builder = builder.user_agent(&user_agent);
    }

    if window.add_child(builder, pos, size).is_err() {
        return false;
    }
    layout::note_created(app, &label, pos, size);

    if let Some(view) = app.get_webview(&label) {
        // Ad/tracker/malware blocking (whatever has compiled so far) + pop-up
        // blocking and fraud warnings, per the user's settings.
        apply_protections(app, &view);
        // Zero-polling URL / back-forward tracking (macOS).
        let kvo_app = app.clone();
        native::observe_nav(&view, Box::new(move |nav| on_nav_state(&kvo_app, id, nav)));
    }

    layout::relayout(app);
    emit_current(app);
    true
}

/// Register + build a brand-new tab; rolls back on failure. Returns the id.
fn open_new(app: &AppHandle, url: String, page: String, incognito: bool) -> Option<usize> {
    let id = register_tab(app, url, page, incognito);
    if build_tab(app, id) {
        Some(id)
    } else {
        unregister_failed(app, id);
        None
    }
}

/// Open a new tab on the bundled home page (private if `incognito`).
pub fn open_home(app: &AppHandle, incognito: bool) {
    open_new(
        app,
        String::new(),
        url_util::NEW_TAB_PAGE.to_string(),
        incognito,
    );
}

/// Open a new tab on a bundled internal page (e.g. settings.html, history.html).
/// If that page is already open in a tab, switch to it instead.
pub fn open_internal(app: &AppHandle, page: &str) {
    let existing = state::lock(app)
        .tabs
        .iter()
        .find(|t| t.is_internal() && t.page == page && !t.incognito)
        .map(|t| t.id);
    match existing {
        Some(id) => select_tab(app, id),
        None => {
            open_new(app, String::new(), page.to_string(), false);
        }
    }
}

/// Open a new tab on an external URL and make it active.
pub fn open_external(app: &AppHandle, url: Url) {
    open_external_as(app, url, false);
}

/// Open a new (optionally private) tab on an external URL and make it active.
pub fn open_external_as(app: &AppHandle, url: Url, incognito: bool) {
    open_new(app, url.to_string(), String::new(), incognito);
}

/// Recreate the last session's tabs. Only the active one is actually loaded;
/// the rest are created *discarded* (title only, no webview/process) and load
/// on first click — so restoring 30 tabs costs the memory of one.
pub fn open_restored(app: &AppHandle, session: Vec<SessionTab>, active: usize) {
    if session.is_empty() {
        open_home(app, false);
        return;
    }
    let active = active.min(session.len() - 1);
    let default_zoom = store::lock(app).settings.default_zoom;
    let active_id = {
        let mut state = state::lock(app);
        let mut active_id = 0;
        for (i, s) in session.into_iter().enumerate() {
            let id = state.allocate_id();
            let mut tab = new_tab_record(default_zoom, id, s.url, s.page, false);
            tab.title = s.title;
            tab.discarded = i != active;
            if i == active {
                active_id = id;
            }
            state.tabs.push(tab);
        }
        state.active = state.index_of(active_id).unwrap_or(0);
        state.note_active();
        active_id
    };
    if !build_tab(app, active_id) {
        // Leave it asleep; it will be rebuilt on click.
        if let Some(t) = state::lock(app).tab_mut(active_id) {
            t.discarded = true;
        }
        emit_current(app);
    }
}

// ---- per-tab webview callbacks ----------------------------------------------

/// Apply a main-frame URL to the tab: internal pages keep the address bar
/// blank (and remember which page they are); web pages update `url`, drop the
/// favicon when the site changes, and get recorded in history (unless
/// incognito). Returns whether the tab's URL changed.
fn apply_url(app: &AppHandle, id: usize, url: &str) -> bool {
    let internal = url_util::is_internal(url);
    // Ignore non-web URLs (about:blank, data:, blob:) so they never clobber the
    // real page URL in the address bar.
    if !(internal || url.starts_with("http") || url.starts_with("file")) {
        return false;
    }
    let mut visit: Option<String> = None;
    let changed = {
        let mut state = state::lock(app);
        let Some(tab) = state.tab_mut(id) else {
            return false;
        };
        if tab.discarded {
            return false;
        }
        if internal {
            let page = url_util::page_of(url).unwrap_or(url_util::NEW_TAB_PAGE);
            let changed = !tab.url.is_empty() || tab.page != page;
            tab.url.clear();
            tab.page = page.to_string();
            tab.favicon.clear();
            changed
        } else if tab.url != url {
            if store::host_of(&tab.url) != store::host_of(url) {
                tab.favicon.clear();
            }
            tab.url = url.to_string();
            tab.page.clear();
            if !tab.incognito {
                visit = Some(tab.url.clone());
            }
            true
        } else {
            false
        }
    };
    if let Some(u) = visit {
        store::lock(app).record_visit(&u, "");
        store::touch(app, Dirty::History);
    }
    changed
}

fn on_title(app: &AppHandle, id: usize, title: String) {
    let mut visit: Option<String> = None;
    {
        let mut state = state::lock(app);
        let Some(tab) = state.tab_mut(id) else {
            return;
        };
        if tab.discarded || tab.title == title {
            return;
        }
        tab.title = title.clone();
        if !tab.url.is_empty() && !tab.incognito {
            visit = Some(tab.url.clone());
        }
    }
    if let Some(u) = visit {
        if store::lock(app).set_title_for(&u, &title) {
            store::touch(app, Dirty::History);
        }
    }
    emit_current(app);
}

/// Native (KVO) navigation-state update: catches single-page-app URL changes
/// (`history.pushState`) that never trigger a page load, plus back/forward
/// availability. Only emits when something actually changed.
fn on_nav_state(app: &AppHandle, id: usize, nav: NavState) {
    let mut changed = {
        let mut state = state::lock(app);
        match state.tab_mut(id) {
            Some(tab)
                if !tab.discarded
                    && (tab.can_back != nav.can_back || tab.can_forward != nav.can_forward) =>
            {
                tab.can_back = nav.can_back;
                tab.can_forward = nav.can_forward;
                true
            }
            _ => false,
        }
    };
    if !nav.url.is_empty() {
        changed |= apply_url(app, id, &nav.url);
    }
    if changed {
        emit_current(app);
    }
}

/// Fallback for platforms without a native URL observer: poll the active tab's
/// `location.href` (see `lib.rs`).
#[cfg(not(target_os = "macos"))]
pub fn poll_active_url(app: &AppHandle) {
    let (id, label) = {
        let state = state::lock(app);
        match state.active_tab() {
            Some(t) if !t.discarded => (t.id, t.label()),
            _ => return,
        }
    };
    let Some(view) = app.get_webview(&label) else {
        return;
    };
    let cb_app = app.clone();
    let _ = view.eval_with_callback("location.href", move |result| {
        if let Ok(url) = serde_json::from_str::<String>(&result) {
            on_nav_state(
                &cb_app,
                id,
                NavState {
                    url,
                    can_back: true,
                    can_forward: true,
                },
            );
        }
    });
}

/// JS that resolves the page's favicon URL (absolute) — `<link rel=icon>` or
/// the conventional `/favicon.ico`. Run once per finished main-frame load.
const FAVICON_JS: &str = r#"(function(){try{var l=document.querySelector('link[rel~="icon"]')||document.querySelector('link[rel="apple-touch-icon"]');return l&&l.href?l.href:(location.origin+'/favicon.ico')}catch(e){return ''}})()"#;

fn on_page_loaded(app: &AppHandle, id: usize, finished: bool, url: String) {
    // Main-frame URL (provisional on start, committed on finish).
    apply_url(app, id, &url);
    let mut apply_zoom: Option<(String, f64)> = None;
    let mut fetch_favicon: Option<String> = None;
    {
        let mut state = state::lock(app);
        let Some(tab) = state.tab_mut(id) else {
            return;
        };
        if tab.discarded {
            return;
        }
        tab.loading = !finished;
        if finished && !tab.url.is_empty() {
            fetch_favicon = Some(tab.label());
        }
        // Re-apply this tab's zoom on every finished load so the default
        // zoom (and any manual ⌘± the user set) survives navigations.
        if finished && (tab.zoom - 1.0).abs() > f64::EPSILON {
            apply_zoom = Some((tab.label(), tab.zoom));
        }
    }
    if let Some((label, factor)) = apply_zoom {
        if let Some(view) = app.get_webview(&label) {
            let _ = view.set_zoom(factor);
        }
    }
    if let Some(label) = fetch_favicon {
        if let Some(view) = app.get_webview(&label) {
            let fav_app = app.clone();
            let _ = view.eval_with_callback(FAVICON_JS, move |result| {
                if let Ok(href) = serde_json::from_str::<String>(&result) {
                    set_favicon(&fav_app, id, href);
                }
            });
        }
    }
    emit_current(app);
}

fn set_favicon(app: &AppHandle, id: usize, href: String) {
    let is_http = href.starts_with("http://") || href.starts_with("https://");
    let is_data = href.starts_with("data:image") && href.len() <= FAVICON_DATA_CAP;
    if !(is_http || is_data) {
        return;
    }
    let mut remember: Option<String> = None;
    let changed = {
        let mut state = state::lock(app);
        match state.tab_mut(id) {
            Some(tab) if !tab.discarded && tab.favicon != href => {
                tab.favicon = href.clone();
                if !tab.incognito {
                    remember = Some(tab.url.clone());
                }
                true
            }
            _ => false,
        }
    };
    if let Some(page_url) = remember {
        // Cache by host so bookmarks/history can show real icons without a
        // third-party favicon service.
        if store::lock(app).set_favicon(&page_url, &href) {
            store::touch(app, Dirty::Favicons);
            emit_chrome_data(app);
        }
    }
    if changed {
        emit_current(app);
    }
}

// ---- hamburger menu (native popup) -----------------------------------------

/// Pop the ☰ menu as a native context menu whose top-RIGHT corner sits at
/// (`right`, `y`) in window logical coordinates — i.e. right-aligned under the
/// button, so it stays inside the window. Native menus cost no webview/process
/// and support keyboard navigation for free.
pub fn show_menu(app: &AppHandle, right: f64, y: f64) {
    let Some(window) = app.get_window("main") else {
        return;
    };
    let x = (right - menu::POPUP_WIDTH).max(4.0);
    if let Ok(menu) = menu::build_popup(app) {
        let _ = window.popup_menu_at(&menu, LogicalPosition::new(x, y));
    }
}

// ---- tab management ---------------------------------------------------------

/// Switch to the tab with the given id. If it was discarded (memory saver), its
/// webview is recreated first — reloading the page it was showing.
pub fn select_tab(app: &AppHandle, id: usize) {
    let revive = {
        let mut state = state::lock(app);
        let Some(i) = state.activate(id) else {
            return;
        };
        let tab = &mut state.tabs[i];
        if tab.discarded {
            tab.discarded = false;
            tab.loading = true;
            true
        } else {
            false
        }
    };
    // `active` already points at the tab, so the new webview is created in
    // place (no create → hide → show round trip).
    if revive && !build_tab(app, id) {
        if let Some(tab) = state::lock(app).tab_mut(id) {
            tab.discarded = true;
            tab.loading = false;
        }
    }
    layout::relayout(app);
    emit_current(app);
    // Give keyboard focus to the page.
    if let Some(view) = app.get_webview(&label_for(id)) {
        let _ = view.set_focus();
    }
}

/// Select the tab at `index` (⌘1…⌘8; ⌘9 = last).
pub fn select_index(app: &AppHandle, index: usize, last: bool) {
    let id = {
        let state = state::lock(app);
        if last {
            state.tabs.last().map(|t| t.id)
        } else {
            state.tabs.get(index).map(|t| t.id)
        }
    };
    if let Some(id) = id {
        select_tab(app, id);
    }
}

/// Cycle to the next/previous tab in strip order.
pub fn select_relative(app: &AppHandle, delta: isize) {
    let id = {
        let state = state::lock(app);
        let n = state.tabs.len() as isize;
        if n == 0 {
            return;
        }
        let i = (state.active as isize + delta).rem_euclid(n) as usize;
        state.tabs[i].id
    };
    select_tab(app, id);
}

/// Menu "Show Next/Previous Tab": by recent use when the MRU switcher is on
/// (one step, applied immediately — there is no modifier to release), else in
/// strip order.
pub fn cycle(app: &AppHandle, delta: isize) {
    if store::lock(app).settings.mru_tab_switching {
        switcher_step(app, delta);
        switcher_commit(app);
    } else {
        select_relative(app, delta);
    }
}

// ---- ⌃⇥ tab switcher (most recently used) -----------------------------------

/// What the chrome renders for the switcher (`null` when it closes).
#[derive(serde::Serialize, Clone)]
struct SwitcherView {
    tabs: Vec<Tab>,
    selected: usize,
}

fn emit_switcher(app: &AppHandle, view: Option<SwitcherView>) {
    let _ = app.emit_to("chrome", "switcher", view);
}

/// Key monitor callback: ⌃⇥ steps the switcher (opening it on the first
/// press), Esc cancels it, releasing ⌃ commits. Returns whether the key was
/// consumed — ⌃⇥ only when MRU switching is on (else the menu's strip-order
/// accelerator handles it), Esc only while the switcher is open.
pub fn on_key(app: &AppHandle, action: native::KeyAction) -> bool {
    match action {
        native::KeyAction::Cycle(delta) => {
            if !store::lock(app).settings.mru_tab_switching {
                return false;
            }
            switcher_step(app, delta);
            true
        }
        native::KeyAction::Escape => switcher_cancel(app),
        native::KeyAction::ControlReleased => {
            switcher_commit(app);
            false
        }
    }
}

/// Open the switcher (previous tab pre-selected) or move its highlight by
/// `delta`. Grows the chrome over the whole window so the overlay can paint
/// above the page. No-op with fewer than two tabs.
pub fn switcher_step(app: &AppHandle, delta: isize) {
    let (view, opened) = {
        let mut state = state::lock(app);
        let was_open = state.switcher.is_some();
        let Some(sw) = state.switcher_step(delta).cloned() else {
            return;
        };
        let tabs = sw
            .ids
            .iter()
            .filter_map(|id| state.tab(*id).cloned())
            .collect();
        if !was_open {
            crate::dbg_log!("switcher open: {:?} selected {}", sw.ids, sw.selected);
        }
        (
            SwitcherView {
                tabs,
                selected: sw.selected,
            },
            !was_open,
        )
    };
    if opened {
        layout::relayout(app);
    }
    emit_switcher(app, Some(view));
}

/// Close the switcher and switch to the highlighted tab (or `pick`).
fn switcher_close(app: &AppHandle, pick: Option<usize>) -> bool {
    let target = {
        let mut state = state::lock(app);
        let Some(sw) = state.switcher.take() else {
            return false;
        };
        pick.or_else(|| sw.selected_id())
    };
    layout::relayout(app);
    emit_switcher(app, None);
    if let Some(id) = target {
        crate::dbg_log!("switcher commit → tab {id}");
        select_tab(app, id);
    }
    true
}

/// Switch to the highlighted tab (⌃ released). Returns whether it was open.
pub fn switcher_commit(app: &AppHandle) -> bool {
    switcher_close(app, None)
}

/// Switch to `id` (a card was clicked).
pub fn switcher_pick(app: &AppHandle, id: usize) {
    switcher_close(app, Some(id));
}

/// Close the switcher without switching (Esc, click outside, focus lost).
pub fn switcher_cancel(app: &AppHandle) -> bool {
    let open = state::lock(app).switcher.take().is_some();
    if open {
        layout::relayout(app);
        emit_switcher(app, None);
    }
    open
}

/// The chrome asks for room over the page for its address-bar dropdown
/// (`None` when it closes). Coordinates are the chrome webview's size.
pub fn set_dropdown_overlay(app: &AppHandle, size: Option<(f64, f64)>) {
    {
        let mut state = state::lock(app);
        if state.dropdown_overlay == size {
            return;
        }
        state.dropdown_overlay = size;
    }
    layout::relayout(app);
}

/// Settings changed the tab arrangement: relayout + repaint.
pub fn set_vertical_tabs(app: &AppHandle, vertical: bool) {
    {
        let mut state = state::lock(app);
        if state.vertical_tabs == vertical {
            return;
        }
        state.vertical_tabs = vertical;
    }
    layout::relayout(app);
}

/// Move the tab `id` to position `to` (drag-reorder in the tab bar).
pub fn move_tab(app: &AppHandle, id: usize, to: usize) {
    if state::lock(app).move_tab(id, to) {
        emit_current(app);
    }
}

/// Close the tab's webview and free the page's memory.
///
/// wry leaks the WKWebView object on close, so the page would otherwise live
/// on inside its WebContent process. We always close the page (`_close`), and
/// when nothing else uses the process — the tab shows external content
/// (internal pages may share the chrome UI's process) and no other live tab or
/// the chrome runs in the same process (WebKit co-hosts same-site tabs) — we
/// have WebKit terminate the process outright. Must be called with no state
/// lock held.
fn teardown_webview(app: &AppHandle, id: usize, external: bool, incognito: bool) {
    let label = label_for(id);
    let others: Vec<String> = state::lock(app)
        .tabs
        .iter()
        .filter(|t| t.id != id && !t.discarded)
        .map(Tab::label)
        .collect();
    let Some(view) = app.get_webview(&label) else {
        return;
    };
    // Take it off screen first so nothing paints/focuses it while it goes.
    let _ = view.hide();
    // Private tab: its cookies/cache/storage live in an ephemeral data store
    // that wry's leaked WKWebView would otherwise keep alive in the Networking
    // process until quit — wipe it now, while we still have the handle.
    if incognito {
        let _ = view.clear_all_browsing_data();
    }
    let mut terminate = external;
    if terminate {
        if let Some(pid) = native::web_process_pid(&view) {
            let mut protected: HashSet<i32> = HashSet::new();
            if let Some(chrome) = app.get_webview("chrome") {
                protected.extend(native::web_process_pid(&chrome));
            }
            for other in others {
                if let Some(v) = app.get_webview(&other) {
                    protected.extend(native::web_process_pid(&v));
                }
            }
            terminate = !protected.contains(&pid);
        } else {
            terminate = false;
        }
    }
    // Drop our KVO observer (it retains the WKWebView) before closing.
    native::forget(&label);
    layout::forget(app, &label);
    // Close the Tauri webview FIRST (drops wry's delegates and Tauri's
    // reload-on-terminate handler), then tear the page down on the retained
    // WKWebView with nothing left to call back into us — and do that on the
    // next run-loop turn, never inside the IPC/menu callout that asked for the
    // close, so WebKit is not re-entered from one of its own callbacks.
    let page = native::retain_page(&view);
    let _ = view.close();
    if let Some(page) = page {
        defer_main(app, move |_| {
            if terminate {
                page.terminate_process();
            }
            page.close();
        });
    }
}

/// Memory saver: free background tabs idle longer than the configured threshold
/// by closing their webview (and terminating its WebContent process when
/// safe). The tab reloads when re-selected. Active, incognito (their session
/// would be lost), and already-discarded tabs are never discarded.
pub fn discard_idle(app: &AppHandle) {
    let threshold_min = store::lock(app).settings.discard_after_min;
    if threshold_min == 0 {
        return;
    }
    let threshold = Duration::from_secs(threshold_min * 60);
    let now = Instant::now();
    let ids: Vec<usize> = {
        let state = state::lock(app);
        state
            .tabs
            .iter()
            .enumerate()
            .filter(|(i, t)| *i != state.active && !t.incognito && !t.discarded)
            .filter(|(_, t)| {
                t.last_active
                    .is_some_and(|la| now.duration_since(la) >= threshold)
            })
            .map(|(_, t)| t.id)
            .collect()
    };
    discard_tabs(app, &ids);
}

/// Discard every background tab right now (Settings ▸ "Free memory now").
pub fn discard_all_background(app: &AppHandle) {
    let ids: Vec<usize> = {
        let state = state::lock(app);
        state
            .tabs
            .iter()
            .enumerate()
            .filter(|(i, t)| *i != state.active && !t.incognito && !t.discarded)
            .map(|(_, t)| t.id)
            .collect()
    };
    discard_tabs(app, &ids);
}

fn discard_tabs(app: &AppHandle, ids: &[usize]) {
    if ids.is_empty() {
        return;
    }
    for id in ids {
        // Mark first so a concurrent process-sharing check sees it as gone.
        let external = {
            let mut state = state::lock(app);
            let Some(tab) = state.tab_mut(*id) else {
                continue;
            };
            tab.discarded = true;
            tab.loading = false;
            tab.can_back = false;
            tab.can_forward = false;
            !tab.is_internal()
        };
        // (Private tabs are never discarded — see the callers' filters.)
        teardown_webview(app, *id, external, false);
    }
    emit_current(app);
}

/// Close the active tab (used by the menu's Close Tab item).
pub fn close_active(app: &AppHandle) {
    let id = state::lock(app).active_tab().map(|t| t.id);
    if let Some(id) = id {
        close_tab(app, id);
    }
}

/// Close the tab with the given id (always keeps at least one tab open). Frees
/// the page's RAM (see `teardown_webview`).
pub fn close_tab(app: &AppHandle, id: usize) {
    let (removed, now_empty, active_id) = {
        let mut state = state::lock(app);
        let removed = state.remove(id).map(|tab| {
            state.remember_closed(&tab);
            (tab.discarded, !tab.is_internal(), tab.incognito)
        });
        (
            removed,
            state.tabs.is_empty(),
            state.active_tab().map(|t| t.id),
        )
    };
    let Some((was_discarded, external, incognito)) = removed else {
        return;
    };
    // Bring the neighbour (or a fresh home tab) forward — it takes focus and
    // covers the closing view — then tear the old page down.
    if now_empty {
        open_home(app, false);
    } else if let Some(aid) = active_id {
        // The newly active tab may be asleep — wake it.
        select_tab(app, aid);
    }
    if !was_discarded {
        teardown_webview(app, id, external, incognito);
    }
}

/// Reopen the most recently closed tab (⌘⇧T).
pub fn reopen_closed(app: &AppHandle) {
    let Some(closed) = state::lock(app).recently_closed.pop() else {
        return;
    };
    if closed.url.is_empty() {
        if closed.page.is_empty() || closed.page == url_util::NEW_TAB_PAGE {
            open_home(app, false);
        } else {
            open_internal(app, &closed.page);
        }
    } else if let Ok(u) = Url::parse(&closed.url) {
        open_external(app, u);
    }
}

/// Navigate the active tab to `url`.
pub fn navigate_active(app: &AppHandle, url: Url) {
    with_active(app, |v| {
        let _ = v.navigate(url);
    });
}

/// Run a snippet of JS in the active tab.
pub fn run_js_active(app: &AppHandle, js: &str) {
    with_active(app, |v| {
        let _ = v.eval(js);
    });
}

pub fn go_back(app: &AppHandle) {
    with_active(app, |v| {
        if !native::go_back(v) {
            let _ = v.eval("history.back()");
        }
    });
}

pub fn go_forward(app: &AppHandle) {
    with_active(app, |v| {
        if !native::go_forward(v) {
            let _ = v.eval("history.forward()");
        }
    });
}

pub fn reload(app: &AppHandle) {
    with_active(app, |v| {
        let _ = v.reload();
    });
}

pub fn stop_loading(app: &AppHandle) {
    with_active(app, |v| {
        if !native::stop_loading(v) {
            let _ = v.eval("window.stop()");
        }
    });
}

pub fn print_active(app: &AppHandle) {
    with_active(app, |v| {
        let _ = v.print();
    });
}

pub fn reload_ignoring_cache(app: &AppHandle) {
    with_active(app, |v| {
        if !native::reload_from_origin(v) {
            let _ = v.eval("location.reload(true)");
        }
    });
}

// ---- developer tools --------------------------------------------------------

/// Open/close the Web Inspector for the active tab (⌥⌘I).
pub fn toggle_devtools(app: &AppHandle) {
    with_active(app, |v| {
        if v.is_devtools_open() {
            v.close_devtools();
        } else {
            v.open_devtools();
        }
    });
}

/// Capture the active page's live DOM and show it in a "View Source" tab.
pub fn view_source(app: &AppHandle) {
    let (label, url) = {
        let state = state::lock(app);
        match state.active_tab() {
            Some(t) if !t.discarded => (t.label(), t.url.clone()),
            _ => return,
        }
    };
    let Some(view) = app.get_webview(&label) else {
        return;
    };
    let cb_app = app.clone();
    let _ = view.eval_with_callback(
        "(function(){try{return document.documentElement.outerHTML}catch(e){return ''}})()",
        move |result| {
            let mut html = serde_json::from_str::<String>(&result).unwrap_or_default();
            if html.len() > SOURCE_CAP {
                let mut cut = SOURCE_CAP;
                while !html.is_char_boundary(cut) {
                    cut -= 1;
                }
                html.truncate(cut);
                html.push_str("\n<!-- … truncated by Foxlite (page source larger than 8 MB) -->\n");
            }
            state::lock(&cb_app).pending_source = Some((url.clone(), html));
            open_new(&cb_app, String::new(), "source.html".into(), false);
        },
    );
}

/// The (url, html) captured by `view_source`, handed to the source page once.
pub fn take_view_source(app: &AppHandle) -> Option<(String, String)> {
    state::lock(app).pending_source.take()
}

/// Apply a new User-Agent to every open tab (new tabs pick it up on creation).
pub fn set_user_agent_all(app: &AppHandle, ua: &str) {
    for view in tab_webviews(app) {
        native::set_user_agent(&view, ua.to_string());
    }
}

/// Wipe cookies, caches and storage of the shared (non-private) session. Any
/// non-incognito webview shares the default data store — including the chrome
/// UI's — so clearing through one clears it for all, even when every tab is
/// private or asleep.
pub fn clear_site_data(app: &AppHandle) {
    let incognito: HashSet<String> = state::lock(app)
        .tabs
        .iter()
        .filter(|t| t.incognito)
        .map(Tab::label)
        .collect();
    let view = tab_webviews(app)
        .into_iter()
        .find(|v| !incognito.contains(v.label()))
        .or_else(|| app.get_webview("chrome"));
    if let Some(v) = view {
        let _ = v.clear_all_browsing_data();
    }
}

/// All live `tab-*` webviews.
fn tab_webviews(app: &AppHandle) -> Vec<tauri::Webview> {
    app.webviews()
        .into_iter()
        .filter(|(label, _)| label.starts_with("tab-"))
        .map(|(_, v)| v)
        .collect()
}

fn with_active(app: &AppHandle, f: impl FnOnce(&tauri::Webview)) {
    let label = state::lock(app).active_tab().map(Tab::label);
    if let Some(view) = label.and_then(|l| app.get_webview(&l)) {
        f(&view);
    }
}

/// JS for `window.find` with `query` safely embedded. A JSON string literal
/// is a valid JS string literal (quotes, backslashes, newlines and U+2028/9
/// are all escaped), so no query can break out of the call.
fn find_js(query: &str, forward: bool) -> String {
    // (U+2028/9 are legal in JSON and, since ES2019, in JS literals too — but
    // escape them anyway so the snippet is safe on any engine.)
    let literal = serde_json::to_string(query)
        .unwrap_or_else(|_| "\"\"".into())
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    let backwards = !forward;
    format!("window.find({literal}, false, {backwards}, true, false, true, false)")
}

/// Find `query` in the active page using the WebView's built-in `window.find`.
pub fn find_in_page(app: &AppHandle, query: &str, forward: bool) {
    run_js_active(app, &find_js(query, forward));
}

pub fn find_clear(app: &AppHandle) {
    run_js_active(app, "window.getSelection().removeAllRanges()");
}

/// Zoom the active tab: `reset` snaps to 1.0, otherwise add `delta` (clamped).
pub fn zoom_active(app: &AppHandle, delta: f64, reset: bool) {
    let info = {
        let mut state = state::lock(app);
        let active = state.active;
        state.tabs.get_mut(active).map(|tab| {
            tab.zoom = if reset {
                1.0
            } else {
                (tab.zoom + delta).clamp(0.3, 3.0)
            };
            (tab.label(), tab.zoom)
        })
    };
    if let Some((label, factor)) = info {
        if let Some(view) = app.get_webview(&label) {
            let _ = view.set_zoom(factor);
        }
        emit_current(app);
    }
}

/// URL and title of the active tab (for bookmarking the current page).
pub fn active_url_title(app: &AppHandle) -> Option<(String, String)> {
    let state = state::lock(app);
    state
        .active_tab()
        .filter(|t| !t.url.is_empty())
        .map(|t| (t.url.clone(), t.title.clone()))
}

// ---- memory report ----------------------------------------------------------

#[derive(serde::Serialize, Clone)]
pub struct TabMemory {
    pub id: usize,
    pub title: String,
    pub bytes: u64,
}

#[derive(serde::Serialize, Clone)]
pub struct MemoryReport {
    pub tabs: Vec<TabMemory>,
    /// Foxlite's own process + the chrome UI's WebContent process.
    pub app_bytes: u64,
    /// Everything above, counting a shared process once.
    pub total_bytes: u64,
    pub available: bool,
}

/// Human label for a tab in the memory list.
fn display_title(t: &Tab) -> String {
    if !t.title.is_empty() {
        t.title.clone()
    } else if t.url.is_empty() {
        "New Tab".to_string()
    } else {
        store::host_of(&t.url).unwrap_or_else(|| t.url.clone())
    }
}

/// Physical memory footprint per tab and overall (macOS; zeros elsewhere).
pub fn memory_report(app: &AppHandle) -> MemoryReport {
    let labels: Vec<(usize, String, String)> = state::lock(app)
        .tabs
        .iter()
        .filter(|t| !t.discarded)
        .map(|t| (t.id, t.label(), display_title(t)))
        .collect();

    let mut seen: HashSet<i32> = HashSet::new();
    let mut total = 0u64;
    // Count a process once; returns its footprint (0 if unknown).
    let mut count = |pid: i32, seen: &mut HashSet<i32>| -> u64 {
        let bytes = native::phys_footprint(pid).unwrap_or(0);
        if seen.insert(pid) {
            total += bytes;
        }
        bytes
    };

    let mut app_bytes = count(native::own_pid(), &mut seen);
    if let Some(pid) = app
        .get_webview("chrome")
        .and_then(|c| native::web_process_pid(&c))
    {
        app_bytes += count(pid, &mut seen);
    }
    let mut tabs = Vec::with_capacity(labels.len());
    for (id, label, title) in labels {
        let Some(pid) = app
            .get_webview(&label)
            .and_then(|v| native::web_process_pid(&v))
        else {
            continue;
        };
        let bytes = count(pid, &mut seen);
        tabs.push(TabMemory { id, title, bytes });
    }
    MemoryReport {
        tabs,
        app_bytes,
        total_bytes: total,
        available: cfg!(target_os = "macos"),
    }
}

// ---- protections (content blocker, pop-ups, fraud warnings) ------------------

/// Rule categories enabled by the current settings.
fn enabled_categories(settings: &Settings) -> HashSet<String> {
    let mut set = HashSet::new();
    if settings.block_ads {
        set.insert(blocklist::CATEGORY_ADS.to_string());
        set.insert(blocklist::CATEGORY_PRIVACY.to_string());
    }
    if settings.block_malware {
        set.insert(blocklist::CATEGORY_SECURITY.to_string());
    }
    if settings.block_annoyances {
        set.insert(blocklist::CATEGORY_ANNOYANCES.to_string());
    }
    set
}

/// Apply the blocker lists + page policies to one webview.
fn apply_protections(app: &AppHandle, view: &tauri::Webview) {
    let (cats, popups, fraud) = {
        let s = store::lock(app);
        (
            enabled_categories(&s.settings),
            s.settings.block_popups,
            s.settings.block_malware,
        )
    };
    native::blocker_apply(view, cats);
    native::set_page_policies(view, popups, fraud);
}

/// Called once the rule lists have compiled: attach them to every open tab.
pub fn on_blocker_ready(app: &AppHandle) {
    apply_protections_all(app);
}

/// Re-apply protections to all open tabs after a settings change (content
/// rules take effect on their next load; pop-up blocking immediately).
pub fn apply_protections_all(app: &AppHandle) {
    for view in tab_webviews(app) {
        apply_protections(app, &view);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_js_escapes_everything() {
        let js = find_js("it's \"q\"\nnext\u{2028}", true);
        assert!(js.starts_with("window.find(\"it's \\\"q\\\"\\n"));
        assert!(!js.contains('\n'), "raw newline would break the literal");
        assert!(!js.contains('\u{2028}'));
        assert!(js.ends_with(", false, false, true, false, true, false)"));
        assert!(find_js("x", false).contains(", false, true, true,"));
    }

    #[test]
    fn categories_follow_settings() {
        let mut s = Settings::default();
        let all = enabled_categories(&s);
        for c in [
            blocklist::CATEGORY_ADS,
            blocklist::CATEGORY_PRIVACY,
            blocklist::CATEGORY_SECURITY,
            blocklist::CATEGORY_ANNOYANCES,
        ] {
            assert!(all.contains(c), "{c} should be on by default");
        }
        s.block_ads = false;
        s.block_annoyances = false;
        let some = enabled_categories(&s);
        assert!(!some.contains(blocklist::CATEGORY_ADS));
        assert!(!some.contains(blocklist::CATEGORY_PRIVACY));
        assert!(some.contains(blocklist::CATEGORY_SECURITY));
        assert!(!some.contains(blocklist::CATEGORY_ANNOYANCES));
    }

    #[test]
    fn download_notice_names_files() {
        let url: Url = "https://example.com/dl/report.pdf?x=1".parse().unwrap();
        let n = download_notice(DownloadEvent::Finished {
            url: url.clone(),
            path: Some(std::path::PathBuf::from("/tmp/report.pdf")),
            success: true,
        })
        .unwrap();
        assert_eq!(
            (n.status, n.name.as_str(), n.path.as_str()),
            ("done", "report.pdf", "/tmp/report.pdf")
        );
        let n = download_notice(DownloadEvent::Requested {
            url,
            destination: &mut std::path::PathBuf::from("/Users/x/Downloads/report.pdf"),
        })
        .unwrap();
        assert_eq!((n.status, n.name.as_str()), ("started", "report.pdf"));
    }

    #[test]
    fn frosted_and_targets() {
        assert!(is_frosted_page("", "settings.html"));
        assert!(!is_frosted_page("", url_util::NEW_TAB_PAGE));
        assert!(!is_frosted_page("https://a.com/", ""));
        assert!(
            matches!(target_for("", "", true), WebviewUrl::App(p) if p.to_string_lossy().contains("incognito=1"))
        );
        assert!(matches!(
            target_for("https://a.com/", "", false),
            WebviewUrl::External(_)
        ));
        assert!(matches!(
            target_for("not a url", "", false),
            WebviewUrl::App(_)
        ));
    }
}
