//! Persistent browser data — settings, bookmarks, history, favicons and the
//! last session. Plain serde + `std::fs`, no plugins, matching the project's
//! dependency-light ethos.
//!
//! Layout on disk (in the OS app-data directory):
//! - `settings.json` — settings + bookmarks (small; rewritten on change)
//! - `session.json` — open tabs for "restore on startup" (small; rewritten)
//! - `history.jsonl` — one visit per line, **append-only** while browsing;
//!   compacted (rewritten) only when old lines changed (late title, trim past
//!   the cap, clear) and then at most every `HISTORY_REWRITE_DELAY`
//! - `favicons.json` — host → icon URL cache (so bookmarks/history never query
//!   a third-party favicon service)
//!
//! Each part has its own dirty flag and debounce; a single background thread
//! (`start_saver`) coalesces changes and writes atomically (temp file +
//! rename). Serialisation happens *outside* the store lock so IPC commands
//! never wait on disk work. `flush` writes everything synchronously at exit.
//! The same thread also drives a periodic tick (used for the memory saver) so
//! the app runs one housekeeping thread instead of several.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

/// Keep at most this many history entries (oldest are dropped past the cap).
const HISTORY_CAP: usize = 5000;
/// Trim in batches so we don't shift the vector on every visit.
const HISTORY_TRIM_AT: usize = HISTORY_CAP + 500;
/// Default UI accent (warm amber). `LEGACY_ACCENT` was the old default; stores
/// still carrying it are moved to the new one on load (a user-picked colour is
/// left alone).
pub const DEFAULT_ACCENT: &str = "#d97b32";
const LEGACY_ACCENT: &str = "#a78bfa";
/// Cap on the favicon cache (hosts).
const FAVICON_CAP: usize = 600;

/// Coalesce windows per kind of data.
const DEBOUNCE_SETTINGS: Duration = Duration::from_millis(400);
const DEBOUNCE_SESSION: Duration = Duration::from_millis(2000);
/// Long enough that a page's title usually arrives before its visit line is
/// appended, so late-title compactions stay rare.
const DEBOUNCE_HISTORY: Duration = Duration::from_millis(3000);
const DEBOUNCE_FAVICONS: Duration = Duration::from_millis(10_000);
/// Minimum spacing between full history-file rewrites while browsing.
const HISTORY_REWRITE_DELAY: Duration = Duration::from_secs(30);

/// User-customizable background for the home page.
/// `kind` is "preset" | "url" | "image"; `value` is the preset id, a remote
/// URL, or — for an uploaded image — a version tag. The image bytes themselves
/// live in `wallpaper.bin` next to the store (see [`save_wallpaper_image`]) and
/// are served to the home page through the `wallpaper://` scheme, so a multi-MB
/// picture is never JSON-encoded, held in memory, or sent over IPC.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Wallpaper {
    pub kind: String,
    pub value: String,
}

impl Default for Wallpaper {
    fn default() -> Self {
        Wallpaper {
            kind: "preset".into(),
            value: "aurora".into(),
        }
    }
}

/// All user preferences. `#[serde(default)]` on the container means older or
/// partial files still load — missing fields fall back to `Default`.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub theme: String,         // "dark" | "light" | "system"
    pub accent: String,        // UI accent colour, hex e.g. "#a78bfa"
    pub search_engine: String, // "duckduckgo" | "google" | "bing" | …
    pub show_bookmarks_bar: bool,
    pub wallpaper: Wallpaper,
    pub startup: String, // "home" | "restore"
    /// Default page zoom for new tabs (1.0 = 100%).
    pub default_zoom: f64,
    /// Memory saver: discard background tabs idle this many minutes (0 = never).
    pub discard_after_min: u64,
    /// Block ads & trackers (EasyList / EasyPrivacy & co.).
    pub block_ads: bool,
    /// Block known malware / phishing hosts and warn on fraudulent sites.
    pub block_malware: bool,
    /// Hide cookie banners and other consent pop-ups.
    pub block_annoyances: bool,
    /// Refuse `window.open` pop-ups that aren't user-initiated.
    pub block_popups: bool,
    /// Developer: enable the Web Inspector (⌥⌘I, right-click ▸ Inspect Element).
    pub devtools: bool,
    /// Developer: custom User-Agent string ("" = the WebView's default).
    pub user_agent: String,
    /// Appearance: opaque window instead of the frosted-glass vibrancy (less
    /// GPU/compositing work). Takes effect on the next launch.
    pub reduce_transparency: bool,
    /// Tabs in a sidebar on the left (with the address bar) instead of a strip
    /// across the top.
    pub vertical_tabs: bool,
    /// ⌃⇥ cycles tabs by most recent use (Arc-style switcher, hold ⌃ to pick)
    /// instead of strip order.
    pub mru_tab_switching: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: "dark".into(),
            accent: DEFAULT_ACCENT.into(),
            search_engine: "duckduckgo".into(),
            show_bookmarks_bar: true,
            wallpaper: Wallpaper::default(),
            startup: "home".into(),
            default_zoom: 1.0,
            discard_after_min: 5,
            block_ads: true,
            block_malware: true,
            block_annoyances: true,
            block_popups: true,
            devtools: true,
            user_agent: String::new(),
            reduce_transparency: false,
            vertical_tabs: false,
            mru_tab_switching: true,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Bookmark {
    pub title: String,
    pub url: String,
}

/// A bookmark plus its cached favicon (for the UI).
#[derive(Clone, Serialize)]
pub struct BookmarkView {
    pub title: String,
    pub url: String,
    pub favicon: String,
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HistoryEntry {
    pub url: String,
    pub title: String,
    pub ts: u64, // epoch seconds
}

/// One page of history for the History UI (newest first).
#[derive(Serialize)]
pub struct HistoryPage {
    pub entries: Vec<HistoryEntry>,
    pub total: usize,
    /// host → favicon URL for the hosts in `entries`.
    pub favicons: HashMap<String, String>,
}

/// One row of the address-bar / home-page suggestion dropdown.
#[derive(Clone, Serialize, PartialEq, Debug)]
pub struct Suggestion {
    /// "search" (a past web search — `query` holds the terms), "bookmark" or
    /// "history".
    pub kind: &'static str,
    pub url: String,
    pub title: String,
    pub favicon: String,
    pub query: String,
}

/// A tab of the last session (for "restore tabs on startup").
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SessionTab {
    pub url: String,
    pub title: String,
    pub page: String,
}

#[derive(Default)]
pub struct Store {
    pub settings: Settings,
    pub bookmarks: Vec<Bookmark>,
    pub history: Vec<HistoryEntry>,
    pub session: Vec<SessionTab>,
    pub session_active: usize,
    /// host → favicon URL (http(s) only).
    pub favicons: HashMap<String, String>,

    // ---- in-memory indexes / bookkeeping (never serialised) ----
    /// Visit count per distinct URL, kept in step with `history` so the
    /// address-bar suggester never rescans the whole list.
    url_counts: HashMap<String, usize>,
    /// `history[history_saved..]` has not been appended to `history.jsonl` yet.
    history_saved: usize,
    /// Lines already on disk were changed (late title, trim, clear): the file
    /// needs a full rewrite. Holds the time it was first flagged.
    history_rewrite_since: Option<Instant>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Host of an http(s) URL, if any.
pub fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
}

impl Store {
    /// Record a page visit. Skips an immediate duplicate of the last entry so
    /// reloads/redirects don't pile up.
    pub fn record_visit(&mut self, url: &str, title: &str) {
        if self.history.last().map(|e| e.url.as_str()) == Some(url) {
            return;
        }
        *self.url_counts.entry(url.to_string()).or_insert(0) += 1;
        self.history.push(HistoryEntry {
            url: url.to_string(),
            title: title.to_string(),
            ts: now_secs(),
        });
        if self.history.len() > HISTORY_TRIM_AT {
            let excess = self.history.len() - HISTORY_CAP;
            for e in self.history.drain(..excess) {
                Self::uncount(&mut self.url_counts, &e.url);
            }
            self.history_saved = self.history_saved.saturating_sub(excess);
            self.flag_rewrite();
        }
    }

    fn uncount(counts: &mut HashMap<String, usize>, url: &str) {
        if let Some(c) = counts.get_mut(url) {
            *c -= 1;
            if *c == 0 {
                counts.remove(url);
            }
        }
    }

    fn flag_rewrite(&mut self) {
        if self.history_rewrite_since.is_none() {
            self.history_rewrite_since = Some(Instant::now());
        }
    }

    /// Fill in the title of the most recent visit to `url` (titles arrive after
    /// the navigation that created the entry). Returns whether anything changed.
    pub fn set_title_for(&mut self, url: &str, title: &str) -> bool {
        if title.is_empty() {
            return false;
        }
        let Some(i) = self.history.iter().rposition(|e| e.url == url) else {
            return false;
        };
        if self.history[i].title == title {
            return false;
        }
        self.history[i].title = title.to_string();
        if i < self.history_saved {
            self.flag_rewrite();
        }
        true
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
        self.url_counts.clear();
        self.history_saved = 0;
        self.flag_rewrite();
    }

    /// Newest-first page of history, optionally filtered by a case-insensitive
    /// substring of URL or title.
    pub fn history_page(&self, offset: usize, limit: usize, query: &str) -> HistoryPage {
        let q = query.trim().to_lowercase();
        let matches = |e: &HistoryEntry| {
            q.is_empty() || e.url.to_lowercase().contains(&q) || e.title.to_lowercase().contains(&q)
        };
        let mut total = 0;
        let mut entries = Vec::with_capacity(limit.min(500));
        for e in self.history.iter().rev() {
            if !matches(e) {
                continue;
            }
            if total >= offset && entries.len() < limit {
                entries.push(e.clone());
            }
            total += 1;
        }
        let mut favicons = HashMap::new();
        for e in &entries {
            if let Some(h) = host_of(&e.url) {
                if let Some(f) = self.favicons.get(&h) {
                    favicons.entry(h).or_insert_with(|| f.clone());
                }
            }
        }
        HistoryPage {
            entries,
            total,
            favicons,
        }
    }

    /// Inline address-bar completion: the best URL (sans scheme) that starts
    /// with `prefix`, from bookmarks and history — e.g. "git" → "github.com".
    /// Bookmarks win, then the most-visited URL, then the shortest candidate.
    /// Allocation-free until the winner is chosen.
    pub fn suggest(&self, prefix: &str) -> Option<String> {
        let p = prefix.trim().to_ascii_lowercase();
        if p.len() < 2 || p.contains(char::is_whitespace) {
            return None;
        }
        fn starts_ci(cand: &str, p: &str) -> bool {
            cand.len() > p.len()
                && cand.is_char_boundary(p.len())
                && cand[..p.len()].eq_ignore_ascii_case(p)
        }
        // Candidate strings a user might be typing: "host" and "host/path".
        fn candidates(url: &str) -> (&str, Option<&str>) {
            let bare = url
                .strip_prefix("https://")
                .or_else(|| url.strip_prefix("http://"))
                .unwrap_or(url);
            let bare = bare.strip_prefix("www.").unwrap_or(bare);
            let bare = bare.strip_suffix('/').unwrap_or(bare);
            let host = bare.split('/').next().unwrap_or(bare);
            (host, (host != bare).then_some(bare))
        }
        // score: (is_bookmark, visit_count, -len)
        type Key = (bool, usize, isize);
        fn consider<'a>(
            best: &mut Option<(Key, &'a str)>,
            p: &str,
            cand: &'a str,
            bm: bool,
            count: usize,
        ) {
            if !starts_ci(cand, p) {
                return;
            }
            let key = (bm, count, -(cand.len() as isize));
            if best.is_none_or(|(k, _)| key > k) {
                *best = Some((key, cand));
            }
        }
        let mut best: Option<(Key, &str)> = None;
        for b in &self.bookmarks {
            let (h, full) = candidates(&b.url);
            consider(&mut best, &p, h, true, 0);
            if let Some(f) = full {
                consider(&mut best, &p, f, true, 0);
            }
        }
        for (url, count) in &self.url_counts {
            let (h, full) = candidates(url);
            consider(&mut best, &p, h, false, *count);
            if let Some(f) = full {
                consider(&mut best, &p, f, false, *count);
            }
        }
        best.map(|(_, s)| s.to_string())
    }

    /// Dropdown suggestions for what the user typed (or, for an empty query,
    /// the most recent distinct pages): bookmarks + history, newest first,
    /// de-duplicated by URL. Typed text matches case-insensitively against
    /// URL and title, every whitespace-separated word must match; candidates
    /// whose host (sans `www.`) starts with the text rank first, then by
    /// visit count, then recency. Past web searches come back as `kind:
    /// "search"` with the search terms in `query`.
    pub fn suggestions(&self, query: &str, limit: usize) -> Vec<Suggestion> {
        let q = query.trim().to_lowercase();
        let words: Vec<&str> = q.split_whitespace().collect();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut out = Vec::new();
        if words.is_empty() {
            for e in self.history.iter().rev() {
                if seen.insert(e.url.as_str()) {
                    out.push(self.suggestion("history", &e.url, &e.title));
                    if out.len() >= limit {
                        break;
                    }
                }
            }
            return out;
        }
        let matches = |url: &str, title: &str| {
            let (u, t) = (url.to_lowercase(), title.to_lowercase());
            words.iter().all(|w| u.contains(w) || t.contains(w))
        };
        // Rank: host-prefix match (2) > bare-URL prefix (1) > substring (0).
        let prefix_rank = |url: &str| -> u8 {
            let bare = url
                .strip_prefix("https://")
                .or_else(|| url.strip_prefix("http://"))
                .unwrap_or(url);
            let bare = bare.strip_prefix("www.").unwrap_or(bare);
            let host = bare.split('/').next().unwrap_or(bare);
            if host.to_lowercase().starts_with(&q) {
                2
            } else if bare.to_lowercase().starts_with(&q) {
                1
            } else {
                0
            }
        };
        // key: (is_bookmark, prefix_rank, visits, last_visit)
        let mut cands: Vec<((bool, u8, usize, u64), Suggestion)> = Vec::new();
        for b in &self.bookmarks {
            if matches(&b.url, &b.title) && seen.insert(b.url.as_str()) {
                cands.push((
                    (true, prefix_rank(&b.url), 0, 0),
                    self.suggestion("bookmark", &b.url, &b.title),
                ));
            }
        }
        for e in self.history.iter().rev() {
            if !seen.contains(e.url.as_str()) && matches(&e.url, &e.title) {
                seen.insert(e.url.as_str());
                let visits = self.url_counts.get(&e.url).copied().unwrap_or(1);
                cands.push((
                    (false, prefix_rank(&e.url), visits, e.ts),
                    self.suggestion("history", &e.url, &e.title),
                ));
            }
        }
        cands.sort_by_key(|c| std::cmp::Reverse(c.0));
        cands.truncate(limit);
        cands.into_iter().map(|(_, s)| s).collect()
    }

    fn suggestion(&self, kind: &'static str, url: &str, title: &str) -> Suggestion {
        let favicon = self.favicon_for(url).unwrap_or_default();
        if let Some(query) = crate::url_util::search_query_of(url) {
            return Suggestion {
                kind: "search",
                url: url.to_string(),
                title: query.clone(),
                favicon,
                query,
            };
        }
        Suggestion {
            kind,
            url: url.to_string(),
            title: title.to_string(),
            favicon,
            query: String::new(),
        }
    }

    pub fn is_bookmarked(&self, url: &str) -> bool {
        self.bookmarks.iter().any(|b| b.url == url)
    }

    pub fn add_bookmark(&mut self, title: &str, url: &str) {
        if !self.is_bookmarked(url) {
            self.bookmarks.push(Bookmark {
                title: title.to_string(),
                url: url.to_string(),
            });
        }
    }

    pub fn remove_bookmark(&mut self, url: &str) {
        self.bookmarks.retain(|b| b.url != url);
    }

    /// Bookmarks with their cached favicons, for the chrome / settings UI.
    pub fn bookmarks_view(&self) -> Vec<BookmarkView> {
        self.bookmarks
            .iter()
            .map(|b| BookmarkView {
                title: b.title.clone(),
                url: b.url.clone(),
                favicon: self.favicon_for(&b.url).unwrap_or_default(),
            })
            .collect()
    }

    /// Cached favicon URL for a page URL's host.
    pub fn favicon_for(&self, url: &str) -> Option<String> {
        host_of(url).and_then(|h| self.favicons.get(&h).cloned())
    }

    /// Remember `icon` (an http(s) URL) for `page_url`'s host. Returns whether
    /// the cache changed. Data URLs and oversized values are not stored.
    pub fn set_favicon(&mut self, page_url: &str, icon: &str) -> bool {
        if !(icon.starts_with("http://") || icon.starts_with("https://")) || icon.len() > 2048 {
            return false;
        }
        let Some(host) = host_of(page_url) else {
            return false;
        };
        if self.favicons.get(&host).map(String::as_str) == Some(icon) {
            return false;
        }
        self.favicons.insert(host, icon.to_string());
        if self.favicons.len() > FAVICON_CAP {
            self.prune_favicons();
        }
        true
    }

    /// Keep icons for bookmarked hosts and the most recently visited hosts.
    fn prune_favicons(&mut self) {
        let mut keep: std::collections::HashSet<String> = self
            .bookmarks
            .iter()
            .filter_map(|b| host_of(&b.url))
            .collect();
        for e in self.history.iter().rev() {
            if keep.len() >= FAVICON_CAP / 2 {
                break;
            }
            if let Some(h) = host_of(&e.url) {
                keep.insert(h);
            }
        }
        self.favicons.retain(|h, _| keep.contains(h));
    }

    /// Rebuild the in-memory indexes after (re)loading history.
    fn reindex(&mut self) {
        self.url_counts.clear();
        for e in &self.history {
            *self.url_counts.entry(e.url.clone()).or_insert(0) += 1;
        }
        self.history_saved = self.history.len();
        self.history_rewrite_since = None;
    }
}

// ---- persistence ------------------------------------------------------------

/// Which part of the store changed (drives what gets written).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dirty {
    Settings,
    Session,
    History,
    Favicons,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct SettingsFile {
    settings: Settings,
    bookmarks: Vec<Bookmark>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct SessionFile {
    tabs: Vec<SessionTab>,
    active: usize,
}

/// Legacy single-file layout (pre-split), read once for migration.
#[derive(Deserialize, Default)]
#[serde(default)]
struct LegacyStore {
    settings: Settings,
    bookmarks: Vec<Bookmark>,
    history: Vec<HistoryEntry>,
    session: Vec<SessionTab>,
    session_active: usize,
}

struct Paths {
    dir: PathBuf,
}

impl Paths {
    fn new(app: &AppHandle) -> Option<Self> {
        Some(Paths {
            dir: app.path().app_data_dir().ok()?,
        })
    }
    fn settings(&self) -> PathBuf {
        self.dir.join("settings.json")
    }
    fn session(&self) -> PathBuf {
        self.dir.join("session.json")
    }
    fn history(&self) -> PathBuf {
        self.dir.join("history.jsonl")
    }
    fn favicons(&self) -> PathBuf {
        self.dir.join("favicons.json")
    }
    fn legacy(&self) -> PathBuf {
        self.dir.join("store.json")
    }
    fn wallpaper(&self) -> (PathBuf, PathBuf) {
        (
            self.dir.join("wallpaper.bin"),
            self.dir.join("wallpaper.mime"),
        )
    }
}

/// Read + parse a JSON file leniently: a missing file yields the default; a
/// corrupt file is moved aside (`<name>.corrupt-<ts>`) so nothing is lost
/// silently, and the default is used.
fn read_json<T: for<'de> Deserialize<'de> + Default>(path: &PathBuf) -> T {
    let Ok(text) = fs::read_to_string(path) else {
        return T::default();
    };
    match serde_json::from_str::<T>(&text) {
        Ok(v) => v,
        Err(e) => {
            crate::dbg_log!("corrupt {}: {e}", path.display());
            let backup = path.with_extension(format!("corrupt-{}", now_secs()));
            let _ = fs::rename(path, backup);
            T::default()
        }
    }
}

/// Serialize + write atomically (best effort; errors are ignored on purpose so
/// a read-only disk never crashes the browser).
fn write_json<T: Serialize>(path: &PathBuf, value: &T) {
    let Ok(json) = serde_json::to_vec(value) else {
        return;
    };
    write_atomic(path, &json);
}

fn write_atomic(path: &PathBuf, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("tmp");
    if fs::write(&tmp, bytes).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

fn read_history(path: &PathBuf) -> Vec<HistoryEntry> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut v: Vec<HistoryEntry> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    if v.len() > HISTORY_CAP {
        let excess = v.len() - HISTORY_CAP;
        v.drain(..excess);
    }
    v
}

fn history_lines(entries: &[HistoryEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(entries.len() * 128);
    for e in entries {
        if let Ok(line) = serde_json::to_vec(e) {
            out.extend_from_slice(&line);
            out.push(b'\n');
        }
    }
    out
}

/// Load the store from disk, falling back to defaults if missing or unreadable.
/// Migrates the legacy single `store.json` (and a wallpaper embedded as a data
/// URL) on first run. Returns the store and whether settings were migrated
/// (so the caller can persist them once the saver is up).
pub fn load(app: &AppHandle) -> (Store, bool) {
    let Some(paths) = Paths::new(app) else {
        return (Store::default(), false);
    };
    let mut store = Store::default();
    let mut migrated = false;

    let legacy_path = paths.legacy();
    if !paths.settings().exists() && legacy_path.exists() {
        let old: LegacyStore = read_json(&legacy_path);
        store.settings = old.settings;
        store.bookmarks = old.bookmarks;
        store.history = old.history;
        store.session = old.session;
        store.session_active = old.session_active;
        store.reindex();
        // Write the new layout now, then keep the old file as a backup.
        write_json(
            &paths.settings(),
            &SettingsFile {
                settings: store.settings.clone(),
                bookmarks: store.bookmarks.clone(),
            },
        );
        write_json(
            &paths.session(),
            &SessionFile {
                tabs: store.session.clone(),
                active: store.session_active,
            },
        );
        write_atomic(&paths.history(), &history_lines(&store.history));
        let _ = fs::rename(&legacy_path, paths.dir.join("store.json.migrated"));
    } else {
        let sf: SettingsFile = read_json(&paths.settings());
        store.settings = sf.settings;
        store.bookmarks = sf.bookmarks;
        let ss: SessionFile = read_json(&paths.session());
        store.session = ss.tabs;
        store.session_active = ss.active;
        store.history = read_history(&paths.history());
        store.favicons = read_json(&paths.favicons());
        store.reindex();
    }

    // Old default accent → new default (only if the user never changed it).
    if store.settings.accent.eq_ignore_ascii_case(LEGACY_ACCENT) {
        store.settings.accent = DEFAULT_ACCENT.into();
        migrated = true;
    }
    // Migrate an old store that embedded the uploaded wallpaper as a data URL.
    if store.settings.wallpaper.kind == "image"
        && store.settings.wallpaper.value.starts_with("data:")
    {
        let data_url = std::mem::take(&mut store.settings.wallpaper.value);
        store.settings.wallpaper = match save_wallpaper_image(app, &data_url) {
            Some(version) => Wallpaper {
                kind: "image".into(),
                value: version,
            },
            None => Wallpaper::default(),
        };
        migrated = true;
    }
    (store, migrated)
}

/// Lock the managed store (poison-tolerant, see `state::lock`).
pub fn lock(app: &AppHandle) -> MutexGuard<'_, Store> {
    app.state::<Mutex<Store>>()
        .inner()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

// ---- uploaded wallpaper -----------------------------------------------------

/// Raster image types we accept for an uploaded wallpaper. SVG is deliberately
/// excluded: it can carry scripts, and `wallpaper://` counts as a local origin.
const WALLPAPER_MIMES: [&str; 5] = [
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
    "image/avif",
];

/// Decode a `data:<mime>;base64,<payload>` URL and write it to disk. Returns
/// the new version tag (used for cache-busting) on success.
pub fn save_wallpaper_image(app: &AppHandle, data_url: &str) -> Option<String> {
    use base64::Engine;
    let rest = data_url.strip_prefix("data:")?;
    let (header, payload) = rest.split_once(',')?;
    if !header.contains(";base64") {
        return None;
    }
    let mime = header
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !WALLPAPER_MIMES.contains(&mime.as_str()) {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .ok()?;
    let (bin, meta) = Paths::new(app)?.wallpaper();
    if let Some(parent) = bin.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(&bin, &bytes).ok()?;
    fs::write(&meta, &mime).ok()?;
    Some(now_secs().to_string())
}

/// The uploaded wallpaper as (mime, bytes), if any. The mime is re-validated
/// against the allow-list so a hand-edited file can't smuggle in another type.
pub fn load_wallpaper_image(app: &AppHandle) -> Option<(String, Vec<u8>)> {
    let (bin, meta) = Paths::new(app)?.wallpaper();
    let bytes = fs::read(bin).ok()?;
    let mime = fs::read_to_string(meta).unwrap_or_else(|_| "image/png".into());
    let mime = if WALLPAPER_MIMES.contains(&mime.as_str()) {
        mime
    } else {
        "image/png".into()
    };
    Some((mime, bytes))
}

// ---- background saver -------------------------------------------------------

#[derive(Default)]
struct Pending {
    /// When each kind was first dirtied (None = clean).
    settings: Option<Instant>,
    session: Option<Instant>,
    history: Option<Instant>,
    favicons: Option<Instant>,
}

impl Pending {
    fn slot(&mut self, kind: Dirty) -> &mut Option<Instant> {
        match kind {
            Dirty::Settings => &mut self.settings,
            Dirty::Session => &mut self.session,
            Dirty::History => &mut self.history,
            Dirty::Favicons => &mut self.favicons,
        }
    }
}

/// Dirty flags + wake-up for the background saver.
pub struct Saver {
    pending: Mutex<Pending>,
    cv: Condvar,
}

fn debounce(kind: Dirty) -> Duration {
    match kind {
        Dirty::Settings => DEBOUNCE_SETTINGS,
        Dirty::Session => DEBOUNCE_SESSION,
        Dirty::History => DEBOUNCE_HISTORY,
        Dirty::Favicons => DEBOUNCE_FAVICONS,
    }
}

/// A periodic job run on the housekeeping thread: (interval, callback).
pub type Tick = (Duration, fn(&AppHandle));

const KINDS: [Dirty; 4] = [
    Dirty::Settings,
    Dirty::Session,
    Dirty::History,
    Dirty::Favicons,
];

/// Start the housekeeping thread. Call once from `setup`. `tick` runs
/// `on_tick` at the given interval on this thread (it should hop to the main
/// thread itself if it needs to touch webviews).
pub fn start_saver(app: &AppHandle, tick: Option<Tick>) {
    let saver = Arc::new(Saver {
        pending: Mutex::new(Pending::default()),
        cv: Condvar::new(),
    });
    app.manage(saver.clone());
    let app = app.clone();
    std::thread::Builder::new()
        .name("foxlite-housekeeping".into())
        .spawn(move || {
            let mut next_tick = tick.map(|(every, _)| Instant::now() + every);
            loop {
                // Decide what's due, or how long to sleep.
                let due: Vec<Dirty> = {
                    let mut p = saver.pending.lock().unwrap_or_else(|e| e.into_inner());
                    loop {
                        let now = Instant::now();
                        let mut deadline: Option<Instant> = next_tick;
                        let mut due = Vec::new();
                        for kind in KINDS {
                            if let Some(since) = *p.slot(kind) {
                                let at = since + debounce(kind);
                                if at <= now {
                                    due.push(kind);
                                } else {
                                    deadline = Some(deadline.map_or(at, |d| d.min(at)));
                                }
                            }
                        }
                        if !due.is_empty() {
                            for k in &due {
                                *p.slot(*k) = None;
                            }
                            break due;
                        }
                        match deadline {
                            Some(d) if d > now => {
                                p = saver
                                    .cv
                                    .wait_timeout(p, d - now)
                                    .unwrap_or_else(|e| e.into_inner())
                                    .0;
                            }
                            Some(_) => {} // a tick is due; fall through and loop again
                            None => {
                                p = saver.cv.wait(p).unwrap_or_else(|e| e.into_inner());
                            }
                        }
                        // Periodic tick (memory saver etc.).
                        if let (Some(t), Some((every, f))) = (next_tick, tick) {
                            if Instant::now() >= t {
                                next_tick = Some(Instant::now() + every);
                                drop(p);
                                f(&app);
                                p = saver.pending.lock().unwrap_or_else(|e| e.into_inner());
                            }
                        }
                    }
                };
                for kind in due {
                    write_kind(&app, kind, false);
                }
            }
        })
        .ok();
}

/// Mark part of the store as changed; it will be written shortly in the
/// background.
pub fn touch(app: &AppHandle, kind: Dirty) {
    if let Some(saver) = app.try_state::<Arc<Saver>>() {
        let mut p = saver.pending.lock().unwrap_or_else(|e| e.into_inner());
        let slot = p.slot(kind);
        if slot.is_none() {
            *slot = Some(Instant::now());
        }
        saver.cv.notify_one();
    } else {
        write_kind(app, kind, true);
    }
}

/// Write everything that is dirty, synchronously (used on exit). Idempotent.
pub fn flush(app: &AppHandle) {
    let due: Vec<Dirty> = match app.try_state::<Arc<Saver>>() {
        Some(saver) => {
            let mut p = saver.pending.lock().unwrap_or_else(|e| e.into_inner());
            let due: Vec<Dirty> = KINDS
                .iter()
                .copied()
                .filter(|k| p.slot(*k).is_some())
                .collect();
            for k in &due {
                *p.slot(*k) = None;
            }
            due
        }
        None => KINDS.to_vec(),
    };
    // A pending history compaction must land too, even if no visit is dirty.
    let needs_history_rewrite = app
        .try_state::<Mutex<Store>>()
        .map(|s| {
            s.lock()
                .unwrap_or_else(|e| e.into_inner())
                .history_rewrite_since
                .is_some()
        })
        .unwrap_or(false);
    for kind in due {
        write_kind(app, kind, true);
    }
    if needs_history_rewrite {
        write_kind(app, Dirty::History, true);
    }
}

/// Serialise (outside the store lock) and write one part of the store.
fn write_kind(app: &AppHandle, kind: Dirty, force_rewrite: bool) {
    let Some(paths) = Paths::new(app) else {
        return;
    };
    let Some(store) = app.try_state::<Mutex<Store>>() else {
        return;
    };
    match kind {
        Dirty::Settings => {
            let file = {
                let s = store.lock().unwrap_or_else(|e| e.into_inner());
                SettingsFile {
                    settings: s.settings.clone(),
                    bookmarks: s.bookmarks.clone(),
                }
            };
            write_json(&paths.settings(), &file);
        }
        Dirty::Session => {
            let file = {
                let s = store.lock().unwrap_or_else(|e| e.into_inner());
                SessionFile {
                    tabs: s.session.clone(),
                    active: s.session_active,
                }
            };
            write_json(&paths.session(), &file);
        }
        Dirty::Favicons => {
            let map = store
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .favicons
                .clone();
            write_json(&paths.favicons(), &map);
        }
        Dirty::History => {
            // Either append the new tail, or (rarely) compact the whole file.
            enum Job {
                Append(Vec<HistoryEntry>),
                Rewrite(Vec<HistoryEntry>),
                Nothing,
            }
            let job = {
                let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
                let rewrite_due = match s.history_rewrite_since {
                    Some(t) => force_rewrite || t.elapsed() >= HISTORY_REWRITE_DELAY,
                    None => false,
                };
                if rewrite_due {
                    s.history_rewrite_since = None;
                    s.history_saved = s.history.len();
                    Job::Rewrite(s.history.clone())
                } else if s.history_saved < s.history.len() {
                    let tail = s.history[s.history_saved..].to_vec();
                    s.history_saved = s.history.len();
                    if s.history_rewrite_since.is_some() {
                        // Keep the compaction pending for later.
                        touch(app, Dirty::History);
                    }
                    Job::Append(tail)
                } else {
                    if s.history_rewrite_since.is_some() {
                        touch(app, Dirty::History);
                    }
                    Job::Nothing
                }
            };
            match job {
                Job::Append(tail) => {
                    let path = paths.history();
                    if let Some(parent) = path.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path)
                    {
                        let _ = f.write_all(&history_lines(&tail));
                    }
                }
                Job::Rewrite(all) => write_atomic(&paths.history(), &history_lines(&all)),
                Job::Nothing => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(urls: &[&str]) -> Store {
        let mut s = Store::default();
        for u in urls {
            s.record_visit(u, "");
        }
        s
    }

    #[test]
    fn suggest_prefers_bookmarks_then_count_then_shortest() {
        let mut s = store_with(&[
            "https://github.com/a",
            "https://gitlab.com/",
            "https://github.com/a",
            "https://www.giant.example/x/y",
        ]);
        assert_eq!(s.suggest("git").as_deref(), Some("github.com"));
        s.add_bookmark("GitLab", "https://gitlab.com/");
        assert_eq!(s.suggest("git").as_deref(), Some("gitlab.com"));
        assert_eq!(s.suggest("gia").as_deref(), Some("giant.example"));
        assert_eq!(
            s.suggest("giant.example/").as_deref(),
            Some("giant.example/x/y")
        );
        assert_eq!(s.suggest("g"), None);
        assert_eq!(s.suggest("GITH").as_deref(), Some("github.com"));
    }

    #[test]
    fn suggestions_recent_then_ranked() {
        let mut s = store_with(&[
            "https://github.com/a",
            "https://duckduckgo.com/?q=rust+lang",
            "https://gitlab.com/",
            "https://github.com/a",
            "https://news.example/git-story",
        ]);
        s.set_title_for("https://news.example/git-story", "A story about git");
        // Empty query: newest first, de-duplicated.
        let recent = s.suggestions("", 10);
        assert_eq!(
            recent.iter().map(|x| x.url.as_str()).collect::<Vec<_>>(),
            [
                "https://news.example/git-story",
                "https://github.com/a",
                "https://gitlab.com/",
                "https://duckduckgo.com/?q=rust+lang",
            ]
        );
        assert_eq!(recent[3].kind, "search");
        assert_eq!(recent[3].query, "rust lang");
        assert_eq!(recent[3].title, "rust lang");
        assert_eq!(s.suggestions("", 2).len(), 2);
        // Typed: host-prefix matches first (github: 2 visits beats gitlab),
        // then the title-only match.
        let git = s.suggestions("git", 10);
        assert_eq!(
            git.iter().map(|x| x.url.as_str()).collect::<Vec<_>>(),
            [
                "https://github.com/a",
                "https://gitlab.com/",
                "https://news.example/git-story",
            ]
        );
        // Bookmarks win; multi-word matches need every word.
        s.add_bookmark("GitLab", "https://gitlab.com/");
        assert_eq!(s.suggestions("git", 10)[0].kind, "bookmark");
        assert_eq!(s.suggestions("story git", 10).len(), 1);
        assert!(s.suggestions("zzz", 10).is_empty());
    }

    #[test]
    fn record_visit_dedups_and_counts() {
        let mut s = store_with(&[
            "https://a.com/",
            "https://a.com/",
            "https://b.com/",
            "https://a.com/",
        ]);
        assert_eq!(s.history.len(), 3);
        assert_eq!(s.url_counts["https://a.com/"], 2);
        assert!(s.set_title_for("https://a.com/", "A"));
        assert_eq!(s.history[2].title, "A");
        assert!(s.history_rewrite_since.is_none()); // unsaved entries: no rewrite
        s.history_saved = 3;
        assert!(s.set_title_for("https://b.com/", "B"));
        assert!(s.history_rewrite_since.is_some());
    }

    #[test]
    fn history_page_filters_and_pages() {
        let s = store_with(&["https://a.com/1", "https://b.com/2", "https://a.com/3"]);
        let p = s.history_page(0, 2, "");
        assert_eq!(p.total, 3);
        assert_eq!(p.entries[0].url, "https://a.com/3");
        let p = s.history_page(1, 10, "A.COM");
        assert_eq!(p.total, 2);
        assert_eq!(p.entries.len(), 1);
        assert_eq!(p.entries[0].url, "https://a.com/1");
    }

    #[test]
    fn favicons_are_http_only_and_capped() {
        let mut s = Store::default();
        assert!(s.set_favicon("https://a.com/x", "https://a.com/favicon.ico"));
        assert!(!s.set_favicon("https://a.com/x", "https://a.com/favicon.ico"));
        assert!(!s.set_favicon("https://a.com/x", "data:image/png;base64,AAAA"));
        assert_eq!(
            s.favicon_for("https://a.com/other").as_deref(),
            Some("https://a.com/favicon.ico")
        );
        for i in 0..FAVICON_CAP + 10 {
            s.set_favicon(
                &format!("https://h{i}.com/"),
                &format!("https://h{i}.com/i.ico"),
            );
        }
        assert!(s.favicons.len() <= FAVICON_CAP);
    }

    #[test]
    fn wallpaper_mime_allowlist() {
        assert!(WALLPAPER_MIMES.contains(&"image/png"));
        assert!(!WALLPAPER_MIMES.contains(&"image/svg+xml"));
    }
}
