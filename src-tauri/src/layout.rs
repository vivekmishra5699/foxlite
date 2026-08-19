//! Positioning + visibility of the chrome strip and the per-tab page webviews.
//!
//! The chrome height is dynamic: the chrome UI measures its real height (which
//! grows when the bookmarks/find bar is shown) and reports it via
//! `set_chrome_height`; we size the chrome webview + offset the tabs to match.
//!
//! Background tabs are **hidden** (`setHidden:` on the NSView), not parked
//! off-screen: WebKit treats an off-screen-but-unhidden view as visible, so its
//! timers, rAF, media and compositing keep running at full rate. A hidden view
//! gets `document.hidden = true`, throttled timers, paused rendering, and
//! background memory-pressure handling — the same thing Safari does with its
//! background tabs.
//!
//! Two arrangements: tabs across the top (chrome = a strip of `chrome_height`
//! over the full width, page below) or vertical tabs (chrome = a sidebar of
//! `SIDEBAR_WIDTH` down the left, page to its right). Either way the chrome is
//! one rectangle, so page and chrome never overlap — except while the chrome
//! shows an **overlay** (address-bar dropdown, ⌃⇥ switcher): then the chrome
//! webview temporarily grows over the page and is raised above the tab views
//! (`native::bring_to_front`), shrinking back when the overlay closes.
//!
//! Frames/visibility are only pushed to a webview when they actually change:
//! every `setFrame` on a (transparent) WKWebView is a repaint, and on macOS a
//! repaint of a layer whose contents aren't ready yet can composite as a black
//! flash. Skipping no-op updates removes most of those triggers.

use std::collections::HashMap;
use std::sync::Mutex;

use tauri::{AppHandle, LogicalPosition, LogicalSize, Manager, Window};

use crate::native;
use crate::state::{self, BrowserState, DEFAULT_CHROME_HEIGHT};

/// Logical (width, height) of the window.
pub fn window_size(window: &Window) -> Option<(f64, f64)> {
    let size = window.inner_size().ok()?;
    let scale = window.scale_factor().ok()?;
    Some((size.width as f64 / scale, size.height as f64 / scale))
}

/// Gap (logical px) left around the page so it floats as an inset "card" on the
/// window frame — the Zen look. The frame colour shows through this margin.
const GAP: f64 = 8.0;

/// Width (logical px) of the chrome sidebar when tabs are vertical. Must match
/// `--sidebar-w` in `styles.css` (the sidebar lays itself out at exactly the
/// webview's width).
pub const SIDEBAR_WIDTH: f64 = 256.0;

/// Position + size of the content region (below the chrome of height `chrome_h`).
/// The page is inset by `GAP` on the left, right and bottom, and sits `GAP`
/// below the chrome, so the frame colour frames it on all sides.
pub fn content_bounds(w: f64, h: f64, chrome_h: f64) -> (LogicalPosition<f64>, LogicalSize<f64>) {
    let x = GAP;
    let y = chrome_h + GAP;
    let width = (w - GAP * 2.0).max(0.0);
    let height = (h - chrome_h - GAP * 2.0).max(0.0);
    (LogicalPosition::new(x, y), LogicalSize::new(width, height))
}

/// Content region with vertical tabs: right of the sidebar, inset by `GAP`
/// on every side so the frame colour shows between sidebar and page too.
pub fn content_bounds_vertical(w: f64, h: f64) -> (LogicalPosition<f64>, LogicalSize<f64>) {
    let width = (w - SIDEBAR_WIDTH - GAP * 2.0).max(0.0);
    let height = (h - GAP * 2.0).max(0.0);
    (
        LogicalPosition::new(SIDEBAR_WIDTH + GAP, GAP),
        LogicalSize::new(width, height),
    )
}

/// The chrome's own (non-overlay) frame and the page frame for a window of
/// `w`×`h`: (chrome size, page position, page size).
pub fn frames(
    state: &BrowserState,
    w: f64,
    h: f64,
) -> (LogicalSize<f64>, LogicalPosition<f64>, LogicalSize<f64>) {
    if state.vertical_tabs {
        let (pos, size) = content_bounds_vertical(w, h);
        (LogicalSize::new(SIDEBAR_WIDTH.min(w), h), pos, size)
    } else {
        let chrome_h = if state.chrome_height > 0.0 {
            state.chrome_height
        } else {
            DEFAULT_CHROME_HEIGHT
        };
        let (pos, size) = content_bounds(w, h, chrome_h);
        (LogicalSize::new(w, chrome_h.min(h)), pos, size)
    }
}

/// Size the chrome webview must have right now: its base frame, grown to
/// cover an open overlay (the whole window for the ⌃⇥ switcher; whatever the
/// dropdown asked for, clamped to the window).
fn chrome_size(state: &BrowserState, base: LogicalSize<f64>, w: f64, h: f64) -> LogicalSize<f64> {
    if state.switcher.is_some() {
        return LogicalSize::new(w, h);
    }
    match state.dropdown_overlay {
        Some((ow, oh)) => LogicalSize::new(ow.max(base.width).min(w), oh.max(base.height).min(h)),
        None => base,
    }
}

/// Last state applied to a webview.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Frame {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    visible: bool,
}

/// Frame cache keyed by webview label.
#[derive(Default)]
pub struct Applied(HashMap<String, Frame>);

/// Register the frame cache. Call once from `setup`.
pub fn init(app: &AppHandle) {
    app.manage(Mutex::new(Applied::default()));
}

fn applied(app: &AppHandle) -> tauri::State<'_, Mutex<Applied>> {
    app.state::<Mutex<Applied>>()
}

/// Forget a webview's cached frame (call when its webview is closed, so a
/// recreated one with the same label is positioned afresh).
pub fn forget(app: &AppHandle, label: &str) {
    applied(app)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .0
        .remove(label);
}

/// Record the frame a webview was *created* with (visible), so the following
/// relayout doesn't re-set (and repaint) an identical frame on the new view.
pub fn note_created(
    app: &AppHandle,
    label: &str,
    pos: LogicalPosition<f64>,
    size: LogicalSize<f64>,
) {
    applied(app)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .0
        .insert(
            label.to_string(),
            Frame {
                x: pos.x,
                y: pos.y,
                w: size.width,
                h: size.height,
                visible: true,
            },
        );
}

/// Show `view` at `pos`/`size`, touching only what changed.
fn place_visible(
    app: &AppHandle,
    view: &tauri::Webview,
    pos: LogicalPosition<f64>,
    size: LogicalSize<f64>,
) {
    let binding = applied(app);
    let mut cache = binding.lock().unwrap_or_else(|e| e.into_inner());
    let prev = cache.0.get(view.label()).copied();
    let want = Frame {
        x: pos.x,
        y: pos.y,
        w: size.width,
        h: size.height,
        visible: true,
    };
    if prev == Some(want) {
        return;
    }
    let same_frame = prev.is_some_and(|p| (p.x, p.y, p.w, p.h) == (want.x, want.y, want.w, want.h));
    if !same_frame {
        let _ = view.set_position(pos);
        let _ = view.set_size(size);
    }
    if prev.is_none_or(|p| !p.visible) {
        let _ = view.show();
    }
    cache.0.insert(view.label().to_string(), want);
}

/// Hide `view` (keeps its last frame so re-showing needs no resize).
fn hide(app: &AppHandle, view: &tauri::Webview) {
    let binding = applied(app);
    let mut cache = binding.lock().unwrap_or_else(|e| e.into_inner());
    match cache.0.get_mut(view.label()) {
        Some(f) if !f.visible => {}
        Some(f) => {
            f.visible = false;
            let _ = view.hide();
        }
        None => {
            let _ = view.hide();
            cache.0.insert(
                view.label().to_string(),
                Frame {
                    x: 0.0,
                    y: 0.0,
                    w: 0.0,
                    h: 0.0,
                    visible: false,
                },
            );
        }
    }
}

/// Resize the chrome and reposition/show/hide all tabs for the given state.
/// (Caller must NOT hold the state lock.)
pub fn apply(app: &AppHandle, state: &BrowserState) {
    let Some(window) = app.get_window("main") else {
        return;
    };
    let Some((w, h)) = window_size(&window) else {
        return;
    };

    let (base, pos, size) = frames(state, w, h);
    let chrome_sz = chrome_size(state, base, w, h);
    let overlay = chrome_sz != base;

    if let Some(chrome) = app.get_webview("chrome") {
        // Newer tab webviews sit above the chrome in the view hierarchy; an
        // overlay has to paint over the page, so raise the chrome first
        // (no-op when it already is on top).
        if overlay {
            native::bring_to_front(&chrome);
        }
        place_visible(app, &chrome, LogicalPosition::new(0.0, 0.0), chrome_sz);
    }

    // Show the active tab first, then hide the rest, so there is never a
    // moment with no page visible.
    if let Some(active) = state.active_tab() {
        if let Some(view) = app.get_webview(&active.label()) {
            place_visible(app, &view, pos, size);
        }
    }
    for (i, tab) in state.tabs.iter().enumerate() {
        if i == state.active || tab.discarded {
            continue;
        }
        if let Some(view) = app.get_webview(&tab.label()) {
            hide(app, &view);
        }
    }
}

/// Lock the state and re-apply layout. Use from event handlers (e.g. resize).
pub fn relayout(app: &AppHandle) {
    let state = state::lock(app);
    apply(app, &state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_follow_mode_and_overlay() {
        let mut s = BrowserState::new();
        s.chrome_height = 100.0;
        let (chrome, pos, size) = frames(&s, 1000.0, 700.0);
        assert_eq!((chrome.width, chrome.height), (1000.0, 100.0));
        assert_eq!((pos.x, pos.y), (GAP, 100.0 + GAP));
        assert_eq!(
            (size.width, size.height),
            (1000.0 - 2.0 * GAP, 700.0 - 100.0 - 2.0 * GAP)
        );
        assert_eq!(chrome_size(&s, chrome, 1000.0, 700.0), chrome);

        s.dropdown_overlay = Some((600.0, 500.0));
        let sz = chrome_size(&s, chrome, 1000.0, 700.0);
        assert_eq!((sz.width, sz.height), (1000.0, 500.0)); // never narrower than the strip
        s.dropdown_overlay = Some((2000.0, 5000.0));
        let sz = chrome_size(&s, chrome, 1000.0, 700.0);
        assert_eq!((sz.width, sz.height), (1000.0, 700.0)); // clamped to the window
        s.dropdown_overlay = None;

        s.vertical_tabs = true;
        let (chrome, pos, size) = frames(&s, 1000.0, 700.0);
        assert_eq!((chrome.width, chrome.height), (SIDEBAR_WIDTH, 700.0));
        assert_eq!((pos.x, pos.y), (SIDEBAR_WIDTH + GAP, GAP));
        assert_eq!(
            (size.width, size.height),
            (1000.0 - SIDEBAR_WIDTH - 2.0 * GAP, 700.0 - 2.0 * GAP)
        );
        s.dropdown_overlay = Some((600.0, 300.0));
        let sz = chrome_size(&s, chrome, 1000.0, 700.0);
        assert_eq!((sz.width, sz.height), (600.0, 700.0)); // grows in width only

        s.switcher = Some(crate::state::Switcher {
            ids: vec![0, 1],
            selected: 1,
        });
        let sz = chrome_size(&s, chrome, 1000.0, 700.0);
        assert_eq!((sz.width, sz.height), (1000.0, 700.0));
    }
}
