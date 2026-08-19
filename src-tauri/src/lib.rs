//! Foxlite — a minimal, memory-light browser built on the OS WebView (Tauri).
//!
//! One window hosts two kinds of webviews:
//!   - "chrome": our HTML/CSS/JS UI (tab bar + toolbar + bookmarks bar) on top
//!   - "tab-{id}": one per open tab. Holds either a bundled internal page
//!     (home/settings/history) or real web content below the chrome.
//!
//! The Rust side owns tab state and positions the webviews; the chrome talks to
//! it over IPC commands and receives `state` / `chrome-data` events.

/// Debug-build logging to stderr (compiled out of release builds; the format
/// arguments are still type-checked and "used" so nothing warns).
macro_rules! dbg_log {
    ($($t:tt)*) => {
        if cfg!(debug_assertions) {
            eprintln!("[foxlite] {}", format!($($t)*));
        }
    };
}
pub(crate) use dbg_log;

mod blocklist;
mod commands;
mod layout;
mod menu;
mod native;
#[cfg(debug_assertions)]
mod selftest;
mod state;
mod store;
mod tabs;
mod url_util;
mod watchdog;

use std::sync::Mutex;
use std::time::Duration;

use tauri::{
    webview::WebviewBuilder, AppHandle, LogicalPosition, Manager, RunEvent, WebviewUrl, Window,
};

use state::BrowserState;

/// How often the memory saver checks for idle background tabs.
const DISCARD_TICK: Duration = Duration::from_secs(30);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(Mutex::new(BrowserState::new()))
        .menu(menu::build)
        .on_menu_event(|app, event| menu::handle(app, event.id().as_ref()))
        .register_uri_scheme_protocol("wallpaper", wallpaper_protocol)
        .invoke_handler(tauri::generate_handler![
            commands::navigate,
            commands::suggest,
            commands::suggestions,
            commands::switcher_pick,
            commands::switcher_cancel,
            commands::set_chrome_overlay,
            commands::toggle_vertical_tabs,
            commands::new_tab,
            commands::new_incognito_tab,
            commands::open_url_new_tab,
            commands::close_tab,
            commands::select_tab,
            commands::move_tab,
            commands::reopen_closed_tab,
            commands::go_back,
            commands::go_forward,
            commands::reload,
            commands::stop_loading,
            commands::request_state,
            commands::open_settings,
            commands::open_history,
            commands::show_menu,
            commands::reveal_path,
            commands::toggle_devtools,
            commands::view_source,
            commands::get_view_source,
            commands::reload_ignoring_cache,
            commands::clear_site_data,
            commands::find_in_page,
            commands::find_clear,
            commands::zoom_in,
            commands::zoom_out,
            commands::zoom_reset,
            commands::set_chrome_height,
            commands::memory_report,
            commands::free_memory,
            commands::get_home_data,
            commands::get_settings,
            commands::set_settings,
            commands::blocker_status,
            commands::system_reduce_transparency,
            commands::blocklist_info,
            commands::set_wallpaper,
            commands::toggle_bookmarks_bar,
            commands::add_bookmark,
            commands::get_bookmarks,
            commands::remove_bookmark,
            commands::bookmark_current,
            commands::get_history,
            commands::clear_history,
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            let (startup, session, session_active, opaque, dark) = init_services(&handle);
            let window = build_window(&handle, opaque, dark)?;
            add_chrome(&handle, &window, opaque, dark)?;
            open_first_tabs(&handle, &startup, session, session_active);
            start_background_work(&handle);
            watch_window(&handle, &window);
            // ⌃⇥ / ⌃⇧⇥ / Esc / ⌃-release for the MRU tab switcher, whichever
            // webview has focus.
            let key_handle = handle.clone();
            native::install_key_monitor(Box::new(move |action| tabs::on_key(&key_handle, action)));
            #[cfg(debug_assertions)]
            selftest::maybe_start(&handle);
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building Foxlite")
        .run(|app, event| {
            // Make sure the last few seconds of history/session hit the disk.
            if matches!(event, RunEvent::ExitRequested { .. } | RunEvent::Exit) {
                store::flush(app);
            }
        });
}

/// Serves the uploaded home-page wallpaper (`wallpaper://localhost/current`)
/// straight from disk, so the image never travels through JSON/IPC. Only that
/// one path is served, only raster types, and the response is sandboxed —
/// custom schemes count as a *local* origin for IPC, so nothing scriptable may
/// ever be served here.
fn wallpaper_protocol(
    ctx: tauri::UriSchemeContext<'_, tauri::Wry>,
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    let not_found = || {
        tauri::http::Response::builder()
            .status(404)
            .body(Vec::new())
            .unwrap_or_default()
    };
    // macOS/Linux: wallpaper://localhost/current — Windows: http://wallpaper.localhost/current
    if request.uri().path().trim_start_matches('/') != "current" {
        return not_found();
    }
    match store::load_wallpaper_image(ctx.app_handle()) {
        Some((mime, bytes)) => tauri::http::Response::builder()
            .header("Content-Type", mime)
            .header("X-Content-Type-Options", "nosniff")
            .header("Content-Security-Policy", "sandbox; default-src 'none'")
            .header("Cache-Control", "max-age=31536000, immutable")
            .body(bytes)
            .unwrap_or_default(),
        None => not_found(),
    }
}

/// Load persisted data and register the managed services (store, saver +
/// memory-saver tick, layout cache, ad blocker). Returns what the rest of
/// setup needs: (startup mode, session, session_active, opaque?, dark?).
fn init_services(handle: &AppHandle) -> (String, Vec<store::SessionTab>, usize, bool, bool) {
    let (store, migrated) = store::load(handle);
    let startup = store.settings.startup.clone();
    let session = store.session.clone();
    let session_active = store.session_active;
    // Frosted glass is a macOS vibrancy feature; elsewhere the window is a
    // plain opaque frame (a transparent window without a blur would just show
    // the desktop through the chrome).
    let opaque = !cfg!(target_os = "macos")
        || store.settings.reduce_transparency
        || native::system_reduce_transparency();
    let dark = store.settings.theme != "light";
    handle
        .state::<Mutex<BrowserState>>()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .vertical_tabs = store.settings.vertical_tabs;
    handle.manage(Mutex::new(store));

    // One housekeeping thread: debounced saves + the 30 s tick that runs the
    // memory saver (webview ops must run on the main thread, so it hops there)
    // and the main-thread stall detector.
    store::start_saver(
        handle,
        Some((DISCARD_TICK, |h: &AppHandle| {
            watchdog::tick(h);
            let inner = h.clone();
            let _ = h.run_on_main_thread(move || tabs::discard_idle(&inner));
        })),
    );
    if migrated {
        store::touch(handle, store::Dirty::Settings);
    }
    layout::init(handle);

    // Ad/tracker rules (EasyList & co.): loaded from WebKit's compiled-list
    // cache when this build's rule set was compiled before, otherwise
    // compiled once (async, a few seconds).
    native::blocker_init(handle, blocklist::rule_set(), tabs::on_blocker_ready);

    (startup, session, session_active, opaque, dark)
}

/// Overlay title bar: the webview fills the whole window (no native title bar
/// stealing height); the traffic lights float over our tab bar, Arc/Helium-
/// style. By default the window is transparent with a macOS vibrancy
/// material, so the chrome strip and the margins around the floating page
/// become frosted glass (the Zen look). With "reduce transparency" (our
/// setting or the OS accessibility preference) the window is a plain opaque
/// frame — no continuous blur/compositing work.
fn build_window(handle: &AppHandle, opaque: bool, dark: bool) -> tauri::Result<Window> {
    #[allow(unused_mut)]
    let mut b = tauri::window::WindowBuilder::new(handle, "main")
        .title("Foxlite")
        .inner_size(1200.0, 800.0)
        .min_inner_size(480.0, 320.0);
    #[cfg(target_os = "macos")]
    {
        b = b
            .title_bar_style(tauri::TitleBarStyle::Overlay)
            .hidden_title(true);
        if !opaque {
            use tauri::window::{Effect, EffectState, EffectsBuilder};
            b = b.transparent(true).effects(
                EffectsBuilder::new()
                    .effect(Effect::HudWindow)
                    .state(EffectState::Active)
                    .build(),
            );
        }
    }
    if opaque {
        b = b.background_color(tabs::frame_color(dark));
    }
    b.build()
}

/// Chrome UI across the top. Disable Tauri's native drag-drop handler so HTML5
/// drop events fire in the chrome (lets you drag a link from a page onto the
/// tab bar to open it in a new tab).
fn add_chrome(handle: &AppHandle, window: &Window, opaque: bool, dark: bool) -> tauri::Result<()> {
    let (w, h) = layout::window_size(window).unwrap_or((1200.0, 800.0));
    let chrome_pos = LogicalPosition::new(0.0, 0.0);
    let (chrome_size, _, _) = layout::frames(&state::lock(handle), w, h);
    // The chrome learns the platform (traffic-light inset, shortcut glyphs)
    // and whether it runs opaque from the query string.
    let query = format!(
        "index.html?os={}{}",
        std::env::consts::OS,
        if opaque { "&opaque=1" } else { "" }
    );
    let mut builder =
        WebviewBuilder::new("chrome", WebviewUrl::App(query.into())).disable_drag_drop_handler();
    if opaque {
        builder = builder.background_color(tabs::frame_color(dark));
    } else {
        // Transparent so the window's vibrancy shows through the chrome
        // (frosted-glass tab bar + toolbar); translucent under-page tint.
        builder = builder.transparent(true).background_color(tabs::FRAME_TINT);
    }
    window.add_child(builder, chrome_pos, chrome_size)?;
    layout::note_created(handle, "chrome", chrome_pos, chrome_size);
    Ok(())
}

/// First tab(s): restore the last session (lazily — only the active tab
/// loads) or open the bundled home page.
fn open_first_tabs(
    handle: &AppHandle,
    startup: &str,
    session: Vec<store::SessionTab>,
    active: usize,
) {
    if startup == "restore" && !session.is_empty() {
        tabs::open_restored(handle, session, active);
    } else {
        tabs::open_home(handle, false);
    }
}

/// Platform fallbacks that need a timer. On macOS the address bar is
/// event-driven (native KVO, see `native.rs`) and the memory-saver tick lives
/// on the housekeeping thread, so nothing runs here.
fn start_background_work(handle: &AppHandle) {
    #[cfg(not(target_os = "macos"))]
    {
        // Low-frequency poll of the active tab's `location.href` for
        // single-page-app URL changes.
        let url_handle = handle.clone();
        std::thread::Builder::new()
            .name("foxlite-url-poll".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_millis(1500));
                let h = url_handle.clone();
                let _ = url_handle.run_on_main_thread(move || tabs::poll_active_url(&h));
            })
            .ok();
    }
    let _ = handle;
}

/// Keep the layout correct as the window resizes; drop the ⌃⇥ switcher if
/// the window loses focus while it is open (the ⌃ release would go elsewhere).
fn watch_window(handle: &AppHandle, window: &Window) {
    let h = handle.clone();
    window.on_window_event(move |event| match event {
        tauri::WindowEvent::Resized(_) => layout::relayout(&h),
        tauri::WindowEvent::Focused(false) => {
            tabs::switcher_cancel(&h);
        }
        _ => {}
    });
}
