//! Native application menu with accelerators. Unlike per-webview `keydown`
//! handlers (which only fire when the chrome webview is focused), menu
//! accelerators work no matter which webview holds focus — so browser
//! shortcuts keep working while you're on a web page.
//!
//! The ☰ toolbar button pops `build_popup` as a native context menu — no extra
//! webview or process, and it dispatches through the same `handle`.

use tauri::menu::{
    Menu, MenuBuilder, MenuItem, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder,
};
use tauri::{AppHandle, Emitter, Runtime};

use crate::{commands, tabs};

fn item<R: Runtime>(
    app: &AppHandle<R>,
    id: &str,
    text: &str,
    accel: &str,
) -> tauri::Result<MenuItem<R>> {
    let mut b = MenuItemBuilder::with_id(id, text);
    if !accel.is_empty() {
        b = b.accelerator(accel);
    }
    b.build(app)
}

/// Build the full menu bar.
pub fn build<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    let app_menu = SubmenuBuilder::new(app, "Foxlite")
        .about(None)
        .separator()
        .item(&item(app, "settings", "Settings…", "CmdOrCtrl+,")?)
        .separator()
        .hide()
        .quit()
        .build()?;

    let file = SubmenuBuilder::new(app, "File")
        .item(&item(app, "new-tab", "New Tab", "CmdOrCtrl+T")?)
        .item(&item(
            app,
            "new-incognito",
            "New Private Tab",
            "Shift+CmdOrCtrl+N",
        )?)
        .item(&item(
            app,
            "reopen-tab",
            "Reopen Closed Tab",
            "Shift+CmdOrCtrl+T",
        )?)
        .separator()
        .item(&item(app, "close-tab", "Close Tab", "CmdOrCtrl+W")?)
        .separator()
        .item(&item(app, "print", "Print…", "CmdOrCtrl+P")?)
        .build()?;

    let edit = SubmenuBuilder::new(app, "Edit")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .separator()
        .item(&item(app, "find", "Find…", "CmdOrCtrl+F")?)
        .item(&item(
            app,
            "focus-address",
            "Open Location…",
            "CmdOrCtrl+L",
        )?)
        .build()?;

    let view = SubmenuBuilder::new(app, "View")
        .item(&item(app, "reload", "Reload", "CmdOrCtrl+R")?)
        .item(&item(app, "stop", "Stop", "CmdOrCtrl+.")?)
        .separator()
        .item(&item(app, "zoom-in", "Zoom In", "CmdOrCtrl+Equal")?)
        .item(&item(app, "zoom-out", "Zoom Out", "CmdOrCtrl+-")?)
        .item(&item(app, "zoom-reset", "Actual Size", "CmdOrCtrl+0")?)
        .separator()
        .item(&item(
            app,
            "toggle-bookmarks",
            "Toggle Bookmarks Bar",
            "Shift+CmdOrCtrl+B",
        )?)
        .item(&item(
            app,
            "toggle-vertical-tabs",
            "Toggle Vertical Tabs",
            "Shift+CmdOrCtrl+S",
        )?)
        .build()?;

    let history = SubmenuBuilder::new(app, "History")
        .item(&item(app, "back", "Back", "CmdOrCtrl+[")?)
        .item(&item(app, "forward", "Forward", "CmdOrCtrl+]")?)
        .separator()
        .item(&item(
            app,
            "show-history",
            "Show All History",
            "CmdOrCtrl+Y",
        )?)
        .build()?;

    let bookmarks = SubmenuBuilder::new(app, "Bookmarks")
        .item(&item(app, "bookmark", "Bookmark This Page", "CmdOrCtrl+D")?)
        .build()?;

    let develop = SubmenuBuilder::new(app, "Develop")
        .item(&item(
            app,
            "devtools",
            "Toggle Web Inspector",
            "Alt+CmdOrCtrl+I",
        )?)
        .item(&item(
            app,
            "view-source",
            "View Page Source",
            "Alt+CmdOrCtrl+U",
        )?)
        .separator()
        .item(&item(
            app,
            "reload-nocache",
            "Reload Ignoring Cache",
            "Shift+CmdOrCtrl+R",
        )?)
        .item(&item(
            app,
            "clear-site-data",
            "Clear Cookies & Site Data",
            "",
        )?)
        .build()?;

    let mut window = SubmenuBuilder::new(app, "Window")
        .minimize()
        .separator()
        .item(&item(app, "next-tab", "Show Next Tab", "Ctrl+Tab")?)
        .item(&item(
            app,
            "prev-tab",
            "Show Previous Tab",
            "Ctrl+Shift+Tab",
        )?)
        .separator();
    // ⌘1…⌘8 jump to a tab; ⌘9 jumps to the last one.
    for n in 1..=9u8 {
        let (id, label) = if n == 9 {
            ("tab-last".to_string(), "Last Tab".to_string())
        } else {
            (format!("tab-{}", n - 1), format!("Tab {n}"))
        };
        window = window.item(&item(app, &id, &label, &format!("CmdOrCtrl+{n}"))?);
    }
    let window = window.build()?;

    MenuBuilder::new(app)
        .item(&app_menu)
        .item(&file)
        .item(&edit)
        .item(&view)
        .item(&history)
        .item(&bookmarks)
        .item(&develop)
        .item(&window)
        .build()
}

/// Approximate rendered width of the ☰ popup (logical px) — used to
/// right-align it under the button so it never spills past the window edge.
/// (Tauri doesn't expose the NSMenu to measure it; the item set is fixed, so
/// this is stable. Slightly generous is safer than too small.)
pub const POPUP_WIDTH: f64 = 232.0;

/// The ☰ dropdown, as a native popup menu. No accelerator hints here (they
/// would widen it); the same items in the menu bar show the shortcuts.
pub fn build_popup<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    MenuBuilder::new(app)
        .item(&item(app, "new-tab", "New Tab", "")?)
        .item(&item(app, "new-incognito", "New Private Tab", "")?)
        .item(&item(app, "reopen-tab", "Reopen Closed Tab", "")?)
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&item(app, "find", "Find in Page…", "")?)
        .item(&item(app, "print", "Print…", "")?)
        .item(&item(app, "toggle-bookmarks", "Toggle Bookmarks Bar", "")?)
        .item(&item(
            app,
            "toggle-vertical-tabs",
            "Toggle Vertical Tabs",
            "",
        )?)
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&item(app, "free-memory", "Sleep Background Tabs", "")?)
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&item(app, "devtools", "Web Inspector", "")?)
        .item(&item(app, "view-source", "View Page Source", "")?)
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&item(app, "show-history", "History", "")?)
        .item(&item(app, "settings", "Settings…", "")?)
        .build()
}

/// Dispatch a menu item by id. UI-only actions (Find, focus address bar) are
/// forwarded to the chrome webview; everything else is handled in Rust.
pub fn handle(app: &AppHandle, id: &str) {
    match id {
        "new-tab" => tabs::open_home(app, false),
        "new-incognito" => tabs::open_home(app, true),
        "reopen-tab" => tabs::reopen_closed(app),
        "close-tab" => tabs::close_active(app),
        "print" => tabs::print_active(app),
        "reload" => tabs::reload(app),
        "stop" => tabs::stop_loading(app),
        "back" => tabs::go_back(app),
        "forward" => tabs::go_forward(app),
        "zoom-in" => tabs::zoom_active(app, 0.1, false),
        "zoom-out" => tabs::zoom_active(app, -0.1, false),
        "zoom-reset" => tabs::zoom_active(app, 0.0, true),
        "show-history" => tabs::open_internal(app, "history.html"),
        "settings" => tabs::open_internal(app, "settings.html"),
        "bookmark" => commands::bookmark_current(app.clone()),
        "toggle-bookmarks" => commands::toggle_bookmarks_bar(app.clone()),
        "toggle-vertical-tabs" => commands::toggle_vertical_tabs(app.clone()),
        "free-memory" => tabs::discard_all_background(app),
        "devtools" => tabs::toggle_devtools(app),
        "view-source" => tabs::view_source(app),
        "reload-nocache" => tabs::reload_ignoring_cache(app),
        "clear-site-data" => tabs::clear_site_data(app),
        "next-tab" => tabs::cycle(app, 1),
        "prev-tab" => tabs::cycle(app, -1),
        "tab-last" => tabs::select_index(app, 0, true),
        "find" | "focus-address" => {
            let _ = app.emit_to("chrome", "menu-action", id);
        }
        other => {
            if let Some(n) = other
                .strip_prefix("tab-")
                .and_then(|n| n.parse::<usize>().ok())
            {
                tabs::select_index(app, n, false);
            }
        }
    }
}
