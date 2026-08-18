//! Pure browser state — the tabs and which one is active. No Tauri types here
//! beyond the `lock` accessor, so the model stays simple and easy to test.

use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use serde::Serialize;
use tauri::{AppHandle, Manager};

/// One open tab. The page itself lives in a Tauri webview labelled `tab-{id}`;
/// this is just the metadata the chrome UI needs to render. An empty `url`
/// means the tab is showing an internal page (home/settings/history), in which
/// case the address bar stays blank like a real browser.
#[derive(Clone, Serialize)]
pub struct Tab {
    pub id: usize,
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub incognito: bool,
    pub zoom: f64,
    /// Memory saver: when true the tab's webview has been closed to free its
    /// WebContent process; it's recreated (reloads its URL) when re-selected.
    pub discarded: bool,
    /// Which bundled internal page this tab shows when `url` is empty
    /// (e.g. "newtab.html", "settings.html"). Lets a discarded internal tab
    /// come back as the same page.
    pub page: String,
    /// Absolute URL of the page's favicon (empty until the page reports one).
    pub favicon: String,
    /// Native back/forward availability (drives the toolbar button state).
    pub can_back: bool,
    pub can_forward: bool,
    /// When this tab last stopped being the active tab (or was created). Used to
    /// decide when to discard. Not sent to the UI.
    #[serde(skip)]
    pub last_active: Option<Instant>,
}

impl Tab {
    /// Label of the webview hosting this tab's page.
    pub fn label(&self) -> String {
        label_for(self.id)
    }

    /// True when the tab shows one of our bundled pages rather than the web.
    pub fn is_internal(&self) -> bool {
        self.url.is_empty()
    }
}

/// Webview label for a tab id.
pub fn label_for(id: usize) -> String {
    format!("tab-{id}")
}

/// A closed tab remembered for "Reopen Closed Tab" (⌘⇧T).
#[derive(Clone)]
pub struct ClosedTab {
    pub url: String,
    pub page: String,
}

/// Default on-screen chrome height (tab bar 40 + toolbar 48). The chrome UI
/// measures its real height and reports it via `set_chrome_height`.
pub const DEFAULT_CHROME_HEIGHT: f64 = 88.0;

/// How many closed tabs to remember for reopening.
const CLOSED_CAP: usize = 20;

/// Largest "View Source" capture we keep in memory (bytes).
pub const SOURCE_CAP: usize = 8 * 1024 * 1024;

#[derive(Default)]
pub struct BrowserState {
    pub tabs: Vec<Tab>,
    pub active: usize,
    /// Live chrome height in logical px, reported by the chrome UI.
    pub chrome_height: f64,
    /// Most-recently-closed last.
    pub recently_closed: Vec<ClosedTab>,
    /// (url, html) captured for the next "View Source" page to display.
    pub pending_source: Option<(String, String)>,
    next_id: usize,
}

impl BrowserState {
    pub fn new() -> Self {
        Self {
            chrome_height: DEFAULT_CHROME_HEIGHT,
            ..Default::default()
        }
    }

    /// Hand out a unique, monotonically increasing tab id.
    pub fn allocate_id(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    /// Mark the currently-active tab as "just deactivated" — its idle clock
    /// (for the memory saver) starts now. Call right before changing `active`.
    pub fn deactivate_current(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.last_active = Some(Instant::now());
        }
    }

    pub fn index_of(&self, id: usize) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    pub fn tab(&self, id: usize) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == id)
    }

    pub fn tab_mut(&mut self, id: usize) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    /// Make `id` the active tab (and start the idle clock of the previous
    /// one). Returns the tab's index, or `None` if there is no such tab.
    pub fn activate(&mut self, id: usize) -> Option<usize> {
        let i = self.index_of(id)?;
        self.deactivate_current();
        self.active = i;
        self.tabs[i].last_active = Some(Instant::now());
        Some(i)
    }

    /// Remove tab `id`, keeping `active` pointing at a sensible neighbour:
    /// closing the active tab selects its right-hand neighbour (or the new
    /// last tab); closing another tab keeps the active one. Returns the
    /// removed tab.
    pub fn remove(&mut self, id: usize) -> Option<Tab> {
        let i = self.index_of(id)?;
        let tab = self.tabs.remove(i);
        if i < self.active {
            self.active -= 1;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
        let active = self.active;
        if let Some(t) = self.tabs.get_mut(active) {
            t.last_active = Some(Instant::now());
        }
        Some(tab)
    }

    /// Move tab `id` to position `to` (clamped), preserving the active tab.
    /// Returns whether anything moved.
    pub fn move_tab(&mut self, id: usize, to: usize) -> bool {
        let Some(from) = self.index_of(id) else {
            return false;
        };
        let to = to.min(self.tabs.len().saturating_sub(1));
        if from == to {
            return false;
        }
        let active_id = self.active_tab().map(|t| t.id);
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        if let Some(aid) = active_id {
            self.active = self.index_of(aid).unwrap_or(0);
        }
        true
    }

    pub fn remember_closed(&mut self, tab: &Tab) {
        if tab.incognito {
            return;
        }
        self.recently_closed.push(ClosedTab {
            url: tab.url.clone(),
            page: tab.page.clone(),
        });
        if self.recently_closed.len() > CLOSED_CAP {
            self.recently_closed.remove(0);
        }
    }
}

/// Lock the managed state. Poisoning is recovered from (the data is plain
/// values, so a panic mid-update can't leave it structurally broken) — with
/// `panic = "abort"` in release this only matters in debug builds anyway.
/// (`State::inner` borrows for as long as `app`, so the guard can be returned.)
pub fn lock(app: &AppHandle) -> MutexGuard<'_, BrowserState> {
    app.state::<Mutex<BrowserState>>()
        .inner()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(id: usize) -> Tab {
        Tab {
            id,
            url: format!("https://{id}.example/"),
            title: String::new(),
            loading: false,
            incognito: false,
            zoom: 1.0,
            discarded: false,
            page: String::new(),
            favicon: String::new(),
            can_back: false,
            can_forward: false,
            last_active: None,
        }
    }

    fn state(n: usize) -> BrowserState {
        let mut s = BrowserState::new();
        for _ in 0..n {
            let id = s.allocate_id();
            s.tabs.push(tab(id));
        }
        s
    }

    #[test]
    fn remove_active_selects_right_neighbour_or_last() {
        let mut s = state(3);
        s.active = 1;
        s.remove(1);
        assert_eq!(s.active, 1);
        assert_eq!(s.active_tab().unwrap().id, 2);
        s.remove(2);
        assert_eq!(s.active, 0);
        assert_eq!(s.active_tab().unwrap().id, 0);
    }

    #[test]
    fn remove_left_of_active_keeps_active_tab() {
        let mut s = state(3);
        s.active = 2;
        s.remove(0);
        assert_eq!(s.active_tab().unwrap().id, 2);
    }

    #[test]
    fn move_preserves_active() {
        let mut s = state(4);
        s.active = 3;
        assert!(s.move_tab(3, 0));
        assert_eq!(s.active, 0);
        assert_eq!(
            s.tabs.iter().map(|t| t.id).collect::<Vec<_>>(),
            [3, 0, 1, 2]
        );
        assert!(!s.move_tab(3, 0));
        assert!(s.move_tab(0, 99)); // clamped to last
        assert_eq!(s.tabs.last().unwrap().id, 0);
    }

    #[test]
    fn closed_tabs_are_capped_and_skip_incognito() {
        let mut s = state(0);
        let mut t = tab(1);
        t.incognito = true;
        s.remember_closed(&t);
        assert!(s.recently_closed.is_empty());
        for i in 0..CLOSED_CAP + 5 {
            s.remember_closed(&tab(i));
        }
        assert_eq!(s.recently_closed.len(), CLOSED_CAP);
    }
}
