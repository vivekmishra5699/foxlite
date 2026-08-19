//! Thin IPC layer: the chrome UI and bundled internal pages call these; each
//! delegates to `tabs` / `store`.

use tauri::{AppHandle, Emitter};

use crate::native::{self, BlockerStatus};
use crate::store::{self, BookmarkView, Dirty, HistoryPage, Settings, Suggestion, Wallpaper};
use crate::{blocklist, state, tabs, url_util};

// ---- navigation / tabs ------------------------------------------------------

#[tauri::command]
pub fn navigate(app: AppHandle, input: String) {
    let engine = store::lock(&app).settings.search_engine.clone();
    if let Some(url) = url_util::to_url(&input, &engine) {
        tabs::navigate_active(&app, url);
    }
}

/// Inline address-bar completion for what the user has typed so far.
#[tauri::command]
pub fn suggest(app: AppHandle, prefix: String) -> Option<String> {
    store::lock(&app).suggest(&prefix)
}

/// Dropdown rows for the address bar / home search box: recent pages when
/// `query` is empty, else matching bookmarks + history (see
/// `Store::suggestions`).
#[tauri::command]
pub fn suggestions(app: AppHandle, query: String, limit: usize) -> Vec<Suggestion> {
    store::lock(&app).suggestions(&query, limit.clamp(1, 20))
}

#[tauri::command]
pub fn new_tab(app: AppHandle) {
    tabs::open_home(&app, false);
}

#[tauri::command]
pub fn new_incognito_tab(app: AppHandle) {
    tabs::open_home(&app, true);
}

/// Open address-bar text / a URL in a brand-new tab (e.g. middle-click a
/// bookmark, or drop a link onto the tab bar).
#[tauri::command]
pub fn open_url_new_tab(app: AppHandle, url: String) {
    let engine = store::lock(&app).settings.search_engine.clone();
    if let Some(url) = url_util::to_url(&url, &engine) {
        tabs::open_external(&app, url);
    }
}

#[tauri::command]
pub fn close_tab(app: AppHandle, id: usize) {
    tabs::close_tab(&app, id);
}

#[tauri::command]
pub fn select_tab(app: AppHandle, id: usize) {
    tabs::select_tab(&app, id);
}

#[tauri::command]
pub fn move_tab(app: AppHandle, id: usize, to: usize) {
    tabs::move_tab(&app, id, to);
}

#[tauri::command]
pub fn reopen_closed_tab(app: AppHandle) {
    tabs::reopen_closed(&app);
}

/// ⌃⇥ switcher: a card was clicked.
#[tauri::command]
pub fn switcher_pick(app: AppHandle, id: usize) {
    tabs::switcher_pick(&app, id);
}

/// ⌃⇥ switcher: clicked outside the cards.
#[tauri::command]
pub fn switcher_cancel(app: AppHandle) {
    tabs::switcher_cancel(&app);
}

#[tauri::command]
pub fn go_back(app: AppHandle) {
    tabs::go_back(&app);
}

#[tauri::command]
pub fn go_forward(app: AppHandle) {
    tabs::go_forward(&app);
}

#[tauri::command]
pub fn reload(app: AppHandle) {
    tabs::reload(&app);
}

#[tauri::command]
pub fn stop_loading(app: AppHandle) {
    tabs::stop_loading(&app);
}

#[tauri::command]
pub fn open_settings(app: AppHandle) {
    tabs::open_internal(&app, "settings.html");
}

#[tauri::command]
pub fn open_history(app: AppHandle) {
    tabs::open_internal(&app, "history.html");
}

/// Pop the ☰ menu (native), right-aligned to window-logical `right`, below `y`.
#[tauri::command]
pub fn show_menu(app: AppHandle, right: f64, y: f64) {
    tabs::show_menu(&app, right, y);
}

/// Reveal a downloaded file in the OS file manager.
#[tauri::command]
pub fn reveal_path(path: String) {
    if path.is_empty() {
        return;
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("-R")
            .arg(&path)
            .spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer")
            .arg(format!("/select,{path}"))
            .spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(dir) = std::path::Path::new(&path).parent() {
            let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
        }
    }
}

// ---- developer --------------------------------------------------------------

#[tauri::command]
pub fn toggle_devtools(app: AppHandle) {
    tabs::toggle_devtools(&app);
}

#[tauri::command]
pub fn view_source(app: AppHandle) {
    tabs::view_source(&app);
}

/// Source page: fetch what `view_source` captured. `[url, html]`; empty when
/// nothing is pending (e.g. the tab was reloaded).
#[tauri::command]
pub fn get_view_source(app: AppHandle) -> (String, String) {
    tabs::take_view_source(&app).unwrap_or_default()
}

#[tauri::command]
pub fn reload_ignoring_cache(app: AppHandle) {
    tabs::reload_ignoring_cache(&app);
}

#[tauri::command]
pub fn clear_site_data(app: AppHandle) {
    tabs::clear_site_data(&app);
}

// ---- find / zoom ------------------------------------------------------------

#[tauri::command]
pub fn find_in_page(app: AppHandle, query: String, forward: bool) {
    tabs::find_in_page(&app, &query, forward);
}

#[tauri::command]
pub fn find_clear(app: AppHandle) {
    tabs::find_clear(&app);
}

#[tauri::command]
pub fn zoom_in(app: AppHandle) {
    tabs::zoom_active(&app, 0.1, false);
}

#[tauri::command]
pub fn zoom_out(app: AppHandle) {
    tabs::zoom_active(&app, -0.1, false);
}

#[tauri::command]
pub fn zoom_reset(app: AppHandle) {
    tabs::zoom_active(&app, 0.0, true);
}

// ---- layout -----------------------------------------------------------------

/// The chrome UI reports its measured height whenever it changes.
#[tauri::command]
pub fn set_chrome_height(app: AppHandle, height: f64) {
    {
        let mut state = state::lock(&app);
        if (state.chrome_height - height).abs() < 0.5 {
            return;
        }
        state.chrome_height = height;
    }
    crate::layout::relayout(&app);
}

/// The chrome's address-bar dropdown needs the chrome webview to reach
/// `width`×`height` (logical px) over the page; 0×0 when it closes.
#[tauri::command]
pub fn set_chrome_overlay(app: AppHandle, width: f64, height: f64) {
    let size = (width > 0.0 && height > 0.0).then_some((width, height));
    tabs::set_dropdown_overlay(&app, size);
}

/// Chrome UI calls this once it has loaded to receive the current state. We also
/// re-apply layout: by now the window is realized, so any startup sizing that
/// didn't "take" is corrected.
#[tauri::command]
pub fn request_state(app: AppHandle) {
    crate::dbg_log!("chrome UI connected");
    crate::layout::relayout(&app);
    tabs::emit_now(&app);
    tabs::emit_chrome_data(&app);
}

// ---- memory -----------------------------------------------------------------

#[tauri::command]
pub fn memory_report(app: AppHandle) -> tabs::MemoryReport {
    tabs::memory_report(&app)
}

/// Put every background tab to sleep right now.
#[tauri::command]
pub fn free_memory(app: AppHandle) {
    tabs::discard_all_background(&app);
}

// ---- home page --------------------------------------------------------------

/// Everything the home page needs in one round-trip.
#[derive(serde::Serialize)]
pub struct HomeData {
    wallpaper: Wallpaper,
    theme: String,
    accent: String,
}

#[tauri::command]
pub fn get_home_data(app: AppHandle) -> HomeData {
    let s = store::lock(&app);
    HomeData {
        wallpaper: s.settings.wallpaper.clone(),
        theme: s.settings.theme.clone(),
        accent: s.settings.accent.clone(),
    }
}

// ---- settings ---------------------------------------------------------------

#[tauri::command]
pub fn get_settings(app: AppHandle) -> Settings {
    store::lock(&app).settings.clone()
}

/// Ad-blocker availability, for the settings page.
#[tauri::command]
pub fn blocker_status() -> BlockerStatus {
    native::blocker_status()
}

/// Whether the OS asks for reduced transparency (the settings page shows the
/// effective value).
#[tauri::command]
pub fn system_reduce_transparency() -> bool {
    native::system_reduce_transparency()
}

#[tauri::command]
pub fn set_settings(app: AppHandle, mut settings: Settings) {
    let (protections_changed, ua_changed, vertical) = {
        let mut s = store::lock(&app);
        // The wallpaper is only ever changed through `set_wallpaper`.
        settings.wallpaper = s.settings.wallpaper.clone();
        if s.settings == settings {
            return;
        }
        let old = &s.settings;
        let protections = old.block_ads != settings.block_ads
            || old.block_malware != settings.block_malware
            || old.block_annoyances != settings.block_annoyances
            || old.block_popups != settings.block_popups;
        let ua = old.user_agent != settings.user_agent;
        s.settings = settings;
        (
            protections,
            ua.then_some(s.settings.user_agent.clone()),
            s.settings.vertical_tabs,
        )
    };
    store::touch(&app, Dirty::Settings);
    if protections_changed {
        tabs::apply_protections_all(&app);
    }
    if let Some(ua) = ua_changed {
        tabs::set_user_agent_all(&app, &ua);
    }
    tabs::set_vertical_tabs(&app, vertical);
    tabs::emit_chrome_data(&app);
    let _ = app.emit("settings-changed", ());
}

/// Flip between the tab strip and the tab sidebar (View menu).
#[tauri::command]
pub fn toggle_vertical_tabs(app: AppHandle) {
    let vertical = {
        let mut s = store::lock(&app);
        s.settings.vertical_tabs = !s.settings.vertical_tabs;
        s.settings.vertical_tabs
    };
    store::touch(&app, Dirty::Settings);
    tabs::set_vertical_tabs(&app, vertical);
    tabs::emit_chrome_data(&app);
    let _ = app.emit("settings-changed", ());
}

/// Rule count + sources of the built-in blocklist (for the settings page).
#[tauri::command]
pub fn blocklist_info() -> blocklist::Meta {
    blocklist::meta().clone()
}

#[tauri::command]
pub fn set_wallpaper(app: AppHandle, kind: String, value: String) {
    // Uploaded images arrive as a data URL; store the bytes on disk and keep
    // only a version tag in settings (see `store::save_wallpaper_image`).
    let wallpaper = if kind == "image" {
        match store::save_wallpaper_image(&app, &value) {
            Some(version) => Wallpaper {
                kind,
                value: version,
            },
            None => return,
        }
    } else {
        Wallpaper { kind, value }
    };
    store::lock(&app).settings.wallpaper = wallpaper;
    store::touch(&app, Dirty::Settings);
    let _ = app.emit("settings-changed", ());
}

/// Flip the bookmarks bar (⌘⇧B).
#[tauri::command]
pub fn toggle_bookmarks_bar(app: AppHandle) {
    {
        let mut s = store::lock(&app);
        s.settings.show_bookmarks_bar = !s.settings.show_bookmarks_bar;
    }
    store::touch(&app, Dirty::Settings);
    tabs::emit_chrome_data(&app);
    let _ = app.emit("settings-changed", ());
}

// ---- bookmarks --------------------------------------------------------------

#[tauri::command]
pub fn add_bookmark(app: AppHandle, title: String, url: String) {
    store::lock(&app).add_bookmark(&title, &url);
    store::touch(&app, Dirty::Settings);
    tabs::emit_chrome_data(&app);
}

/// All saved bookmarks (for the settings-page bookmarks manager).
#[tauri::command]
pub fn get_bookmarks(app: AppHandle) -> Vec<BookmarkView> {
    store::lock(&app).bookmarks_view()
}

#[tauri::command]
pub fn remove_bookmark(app: AppHandle, url: String) {
    store::lock(&app).remove_bookmark(&url);
    store::touch(&app, Dirty::Settings);
    tabs::emit_chrome_data(&app);
}

/// Toggle a bookmark for the active tab's current page.
#[tauri::command]
pub fn bookmark_current(app: AppHandle) {
    let Some((url, title)) = tabs::active_url_title(&app) else {
        return;
    };
    {
        let mut s = store::lock(&app);
        if s.is_bookmarked(&url) {
            s.remove_bookmark(&url);
        } else {
            let title = if title.is_empty() { url.clone() } else { title };
            s.add_bookmark(&title, &url);
        }
    }
    store::touch(&app, Dirty::Settings);
    tabs::emit_chrome_data(&app);
}

// ---- history ----------------------------------------------------------------

/// A page of history, newest first, optionally filtered by `query`.
#[tauri::command]
pub fn get_history(app: AppHandle, offset: usize, limit: usize, query: String) -> HistoryPage {
    store::lock(&app).history_page(offset, limit.clamp(1, 1000), &query)
}

#[tauri::command]
pub fn clear_history(app: AppHandle) {
    store::lock(&app).clear_history();
    store::touch(&app, Dirty::History);
}
