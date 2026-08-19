//! Native hooks on the platform webview. On macOS we reach the `WKWebView`
//! behind each Tauri webview to do things Tauri's portable API can't:
//!
//!   - observe `URL` / `canGoBack` / `canGoForward` via KVO, so the address bar
//!     tracks single-page-app navigations with **zero polling** (no timer runs
//!     JS in the page; the only JS we ever eval is a one-shot favicon lookup
//!     after a page finishes loading);
//!   - read the exact WebContent process id backing a webview
//!     (`_webProcessIdentifier`) so the memory saver knows which tabs share a
//!     process, and terminate that process through WebKit itself
//!     (`_killWebContentProcessAndResetState`) instead of a raw `kill(2)`;
//!   - read a process's physical memory footprint (per-tab memory readout);
//!   - install a compiled `WKContentRuleList` (ad/tracker blocker) — the single
//!     biggest lever for page RAM and load speed;
//!   - drive back/forward/stop natively;
//!   - read the OS "reduce transparency" accessibility preference.
//!
//! Every function is a no-op / `None` on other platforms so the rest of the app
//! stays portable; callers fall back to the JS-based behaviour there.

/// Snapshot of navigation state read from the native webview.
#[derive(Clone)]
pub struct NavState {
    pub url: String,
    pub can_back: bool,
    pub can_forward: bool,
}

pub type NavHandler = Box<dyn Fn(NavState) + Send + 'static>;

/// Keyboard events the app-wide key monitor reports (see `install_key_monitor`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    /// ⌃⇥ (+1) / ⌃⇧⇥ (−1).
    Cycle(isize),
    /// Escape.
    Escape,
    /// The Control key was released (any flags change that leaves it up).
    ControlReleased,
}

/// Returns whether the event was consumed (then it reaches neither the menu
/// nor the focused webview). Always called on the main thread.
pub type KeyHandler = Box<dyn Fn(KeyAction) -> bool + 'static>;

/// State of the ad/tracker blocker rule list.
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub enum BlockerStatus {
    /// Compiled and attachable.
    Ready,
    /// Still compiling (first launch / list changed).
    Compiling,
    /// WebKit rejected the rule list.
    Failed,
    /// No content-blocker support on this platform.
    #[allow(dead_code)]
    Unsupported,
}

#[cfg(target_os = "macos")]
mod imp {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use std::rc::Rc;

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject, Sel};
    use objc2::{
        class, define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadMarker,
    };
    use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags, NSEventType, NSView};
    use objc2_foundation::{
        ns_string, NSDictionary, NSError, NSKeyValueChangeKey, NSKeyValueObservingOptions,
        NSObjectNSKeyValueObserverRegistration, NSObjectProtocol, NSString,
    };
    use objc2_web_kit::{WKContentRuleList, WKContentRuleListStore, WKWebView};
    use tauri::{AppHandle, Webview};

    use super::{BlockerStatus, KeyAction, KeyHandler, NavHandler, NavState};
    use crate::blocklist::RuleSet;

    // ---- helpers ------------------------------------------------------------

    /// Run `f` with the WKWebView behind `webview`. Tauri dispatches to the
    /// main thread; when we're already on it (all our callers are) this runs
    /// synchronously, so out-params filled by `f` are readable afterwards.
    fn with_view<F: FnOnce(&WKWebView) + Send + 'static>(webview: &Webview, f: F) {
        let _ = webview.with_webview(move |pw| {
            let ptr = pw.inner() as *mut WKWebView;
            if !ptr.is_null() {
                // SAFETY: wry hands us a live WKWebView pointer on the main thread.
                f(unsafe { &*ptr });
            }
        });
    }

    fn read_nav(view: &WKWebView) -> NavState {
        unsafe {
            NavState {
                url: view
                    .URL()
                    .and_then(|u| u.absoluteString())
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                can_back: view.canGoBack(),
                can_forward: view.canGoForward(),
            }
        }
    }

    // ---- KVO navigation observer ------------------------------------------

    struct Ivars {
        view: Retained<WKWebView>,
        handler: NavHandler,
    }

    /// KVO key paths we observe.
    fn keys() -> [&'static NSString; 3] {
        [
            ns_string!("URL"),
            ns_string!("canGoBack"),
            ns_string!("canGoForward"),
        ]
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "FoxliteNavObserver"]
        #[ivars = Ivars]
        struct NavObserver;

        impl NavObserver {
            #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
            fn observe_value(
                &self,
                _key_path: Option<&NSString>,
                _of_object: Option<&AnyObject>,
                _change: Option<&NSDictionary<NSKeyValueChangeKey, AnyObject>>,
                _context: *mut c_void,
            ) {
                let ivars = self.ivars();
                (ivars.handler)(read_nav(&ivars.view));
            }
        }

        unsafe impl NSObjectProtocol for NavObserver {}
    );

    impl NavObserver {
        fn new(view: Retained<WKWebView>, handler: NavHandler) -> Retained<Self> {
            let this = Self::alloc().set_ivars(Ivars { view, handler });
            let this: Retained<Self> = unsafe { msg_send![super(this), init] };
            unsafe {
                for key in keys() {
                    this.ivars().view.addObserver_forKeyPath_options_context(
                        &this,
                        key,
                        NSKeyValueObservingOptions::New,
                        null_mut(),
                    );
                }
            }
            this
        }
    }

    impl Drop for NavObserver {
        fn drop(&mut self) {
            unsafe {
                for key in keys() {
                    self.ivars().view.removeObserver_forKeyPath(self, key);
                }
            }
        }
    }

    thread_local! {
        // Main-thread registry of live observers keyed by webview label. The
        // observer retains its WKWebView, so it MUST be dropped (`forget`)
        // before the webview is closed or the page's process would linger.
        static OBSERVERS: RefCell<HashMap<String, Retained<NavObserver>>> =
            RefCell::new(HashMap::new());
        /// (category, compiled list) pairs, attached per the user's toggles.
        static RULES: RefCell<Vec<(String, Retained<WKContentRuleList>)>> = const { RefCell::new(Vec::new()) };
        static BLOCKER: Cell<BlockerStatus> = const { Cell::new(BlockerStatus::Compiling) };
        /// The installed NSEvent local monitor (kept alive for the app's lifetime).
        static KEY_MONITOR: RefCell<Option<Retained<AnyObject>>> = const { RefCell::new(None) };
    }

    pub fn observe_nav(webview: &Webview, handler: NavHandler) {
        let label = webview.label().to_string();
        with_view(webview, move |view| {
            // SAFETY: `view` is a live object; retain it for the observer.
            let Some(retained) = (unsafe { Retained::retain(view as *const _ as *mut WKWebView) })
            else {
                return;
            };
            let observer = NavObserver::new(retained, handler);
            OBSERVERS.with(|o| o.borrow_mut().insert(label, observer));
        });
    }

    pub fn forget(label: &str) {
        OBSERVERS.with(|o| o.borrow_mut().remove(label));
    }

    // ---- view order ---------------------------------------------------------

    /// Put the webview's NSView above its siblings (child webviews stack in
    /// creation order, so a later tab would otherwise cover the chrome's
    /// overlays). No-op when it is already the topmost subview. Reorders in
    /// place (`sortSubviews`) rather than removing/re-adding the view, so
    /// WebKit never sees the view leave the window.
    pub fn bring_to_front(webview: &Webview) {
        let label = webview.label().to_string();
        with_view(webview, move |view| {
            let ns: &NSView = view;
            let Some(parent) = (unsafe { ns.superview() }) else {
                return;
            };
            let subs = parent.subviews();
            if subs
                .lastObject()
                .is_some_and(|last| std::ptr::eq(&*last as *const NSView, ns as *const NSView))
            {
                return;
            }
            unsafe extern "C-unwind" fn last_wins(
                a: std::ptr::NonNull<NSView>,
                b: std::ptr::NonNull<NSView>,
                target: *mut c_void,
            ) -> objc2_foundation::NSComparisonResult {
                use objc2_foundation::NSComparisonResult::*;
                if a.as_ptr() as *mut c_void == target {
                    Descending
                } else if b.as_ptr() as *mut c_void == target {
                    Ascending
                } else {
                    Same
                }
            }
            unsafe {
                parent.sortSubviewsUsingFunction_context(
                    last_wins,
                    ns as *const NSView as *mut c_void,
                );
            }
            crate::dbg_log!(
                "raised {label} above {} sibling view(s) (now topmost: {})",
                subs.len() - 1,
                parent
                    .subviews()
                    .lastObject()
                    .is_some_and(|last| std::ptr::eq(&*last as *const NSView, ns as *const NSView))
            );
        });
    }

    // ---- key monitor --------------------------------------------------------

    /// Watch ⌃⇥ / ⌃⇧⇥ / Esc / Control-release app-wide, whichever webview has
    /// focus — a local NSEvent monitor sees key events before the menu and
    /// before WebKit, and can swallow them. Drives the MRU tab switcher (hold
    /// ⌃, tap ⇥ to move, release ⌃ to switch). Install once, on the main
    /// thread.
    pub fn install_key_monitor(handler: KeyHandler) {
        const KEY_TAB: u16 = 48;
        const KEY_ESC: u16 = 53;
        let block = RcBlock::new(move |ev: std::ptr::NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: AppKit hands us a live event for the duration of the call.
            let event = unsafe { ev.as_ref() };
            let flags = event
                .modifierFlags()
                .intersection(NSEventModifierFlags::DeviceIndependentFlagsMask);
            let consumed = match event.r#type() {
                NSEventType::KeyDown => {
                    let code = event.keyCode();
                    let ctrl_only = flags.contains(NSEventModifierFlags::Control)
                        && !flags.contains(NSEventModifierFlags::Command)
                        && !flags.contains(NSEventModifierFlags::Option);
                    if code == KEY_TAB && ctrl_only {
                        let delta = if flags.contains(NSEventModifierFlags::Shift) {
                            -1
                        } else {
                            1
                        };
                        handler(KeyAction::Cycle(delta))
                    } else if code == KEY_ESC {
                        handler(KeyAction::Escape)
                    } else {
                        false
                    }
                }
                NSEventType::FlagsChanged => {
                    if !flags.contains(NSEventModifierFlags::Control) {
                        handler(KeyAction::ControlReleased);
                    }
                    false // modifier changes always pass through
                }
                _ => false,
            };
            if consumed {
                null_mut()
            } else {
                ev.as_ptr()
            }
        });
        let monitor = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(
                NSEventMask::KeyDown | NSEventMask::FlagsChanged,
                &block,
            )
        };
        KEY_MONITOR.with(|m| *m.borrow_mut() = monitor);
    }

    // ---- process / memory ---------------------------------------------------

    pub fn web_process_pid(webview: &Webview) -> Option<i32> {
        let out = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
        let o = out.clone();
        with_view(webview, move |view| {
            // Private but long-stable WKWebView property (WKWebViewPrivate.h).
            let sel: Sel = sel!(_webProcessIdentifier);
            if view.respondsToSelector(sel) {
                let pid: libc::pid_t = unsafe { msg_send![view, _webProcessIdentifier] };
                o.store(pid, std::sync::atomic::Ordering::SeqCst);
            }
        });
        match out.load(std::sync::atomic::Ordering::SeqCst) {
            0 => None,
            pid => Some(pid),
        }
    }

    /// A retained reference to the WKWebView behind a tab, taken *before* the
    /// Tauri webview is closed so the page can be torn down *after* it.
    ///
    /// Order matters: Tauri's runtime installs a "WebContent process
    /// terminated" delegate that **reloads the page**, and wry's navigation
    /// delegate would still route load/title callbacks to our tab. Both are
    /// dropped when the Tauri webview closes (wry only leaks the WKWebView
    /// object itself), so terminating/closing the page on this handle
    /// afterwards is callback-free and cannot re-enter our state.
    pub struct PageHandle(Retained<WKWebView>);
    // SAFETY: the handle is created on the main thread and only ever used from
    // a `run_on_main_thread` closure; it merely travels through the async
    // runtime's queue in between (no method is called off the main thread).
    unsafe impl Send for PageHandle {}

    pub fn retain_page(webview: &Webview) -> Option<PageHandle> {
        // `Retained` isn't Send, so hand the pointer out of the (synchronous,
        // main-thread) closure as an address and retain it here.
        let out = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let o = out.clone();
        with_view(webview, move |view| {
            o.store(
                view as *const WKWebView as usize,
                std::sync::atomic::Ordering::SeqCst,
            );
        });
        let ptr = out.load(std::sync::atomic::Ordering::SeqCst) as *mut WKWebView;
        if ptr.is_null() {
            return None;
        }
        // SAFETY: `ptr` came from a live WKWebView on this (main) thread a
        // moment ago; retaining it keeps the object valid past wry's drop.
        unsafe { Retained::retain(ptr) }.map(PageHandle)
    }

    impl PageHandle {
        /// Ask WebKit to terminate the WebContent process behind the page and
        /// forget its state. Unlike a raw `kill(2)` WebKit knows the process
        /// is gone (no crash report, no stale process-cache entry). Only call
        /// when no other live webview shares the process.
        pub fn terminate_process(&self) -> bool {
            let sel: Sel = sel!(_killWebContentProcessAndResetState);
            if self.0.respondsToSelector(sel) {
                let _: () = unsafe { msg_send![&*self.0, _killWebContentProcessAndResetState] };
                true
            } else {
                false
            }
        }

        /// Close the page (`-[WKWebView _close]`): frees its DOM, JS heap and
        /// network state inside the WebContent process even when that process
        /// is shared with another tab and must stay alive. wry deliberately
        /// leaks the WKWebView object on close (see `InnerWebView::drop`), so
        /// without this a closed tab's page would live on in a shared process.
        pub fn close(&self) {
            let sel: Sel = sel!(_close);
            if self.0.respondsToSelector(sel) {
                let _: () = unsafe { msg_send![&*self.0, _close] };
            }
        }
    }

    pub fn phys_footprint(pid: i32) -> Option<u64> {
        let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::proc_pid_rusage(
                pid,
                libc::RUSAGE_INFO_V2,
                &mut info as *mut libc::rusage_info_v2 as *mut libc::rusage_info_t,
            )
        };
        (rc == 0).then_some(info.ri_phys_footprint)
    }

    pub fn own_pid() -> i32 {
        unsafe { libc::getpid() }
    }

    // ---- native navigation --------------------------------------------------

    pub fn go_back(webview: &Webview) -> bool {
        with_view(webview, |v| unsafe {
            let _ = v.goBack();
        });
        true
    }

    pub fn go_forward(webview: &Webview) -> bool {
        with_view(webview, |v| unsafe {
            let _ = v.goForward();
        });
        true
    }

    pub fn stop_loading(webview: &Webview) -> bool {
        with_view(webview, |v| unsafe { v.stopLoading() });
        true
    }

    /// Reload bypassing the cache (⇧⌘R).
    pub fn reload_from_origin(webview: &Webview) -> bool {
        with_view(webview, |v| unsafe {
            let _ = v.reloadFromOrigin();
        });
        true
    }

    /// Change the User-Agent of a live webview ("" restores the default).
    pub fn set_user_agent(webview: &Webview, ua: String) {
        with_view(webview, move |v| unsafe {
            let ns = (!ua.is_empty()).then(|| NSString::from_str(&ua));
            v.setCustomUserAgent(ns.as_deref());
        });
    }

    // ---- content blocker ----------------------------------------------------

    /// Progress of a batch of async WebKit store operations (lookups or
    /// compiles); the completion handlers all run on the main thread.
    struct Batch {
        results: Vec<Option<Retained<WKContentRuleList>>>,
        done: usize,
    }

    fn install(
        app: &AppHandle,
        lists: Vec<(String, Retained<WKContentRuleList>)>,
        on_ready: fn(&AppHandle),
    ) {
        RULES.with(|r| *r.borrow_mut() = lists);
        BLOCKER.with(|b| b.set(BlockerStatus::Ready));
        on_ready(app);
    }

    /// Compile every chunk of the fallback list as a last resort.
    fn compile_fallback(
        store: Retained<WKContentRuleListStore>,
        app: AppHandle,
        set: Rc<RuleSet>,
        on_ready: fn(&AppHandle),
    ) {
        let json = NSString::from_str(&(set.fallback_json)());
        let ident = NSString::from_str(&set.fallback_identifier);
        let handler = RcBlock::new(move |list: *mut WKContentRuleList, err: *mut NSError| {
            if list.is_null() {
                if !err.is_null() {
                    let desc = unsafe { (*err).localizedDescription() };
                    crate::dbg_log!("fallback blocklist compile failed: {desc}");
                }
                BLOCKER.with(|b| b.set(BlockerStatus::Failed));
                return;
            }
            crate::dbg_log!("using fallback blocklist");
            if let Some(list) = unsafe { Retained::retain(list) } {
                install(
                    &app,
                    vec![(crate::blocklist::CATEGORY_ADS.to_string(), list)],
                    on_ready,
                );
            }
        });
        unsafe {
            store.compileContentRuleListForIdentifier_encodedContentRuleList_completionHandler(
                Some(&ident),
                Some(&json),
                Some(&handler),
            );
        }
    }

    /// Compile all chunks; on any failure fall back to the built-in list.
    fn compile_all(
        store: Retained<WKContentRuleListStore>,
        app: AppHandle,
        set: Rc<RuleSet>,
        on_ready: fn(&AppHandle),
    ) {
        let chunks = (set.chunks)();
        let n = chunks.len();
        if n == 0 || n != set.categories.len() {
            compile_fallback(store, app, set, on_ready);
            return;
        }
        crate::dbg_log!("blocklist not cached; compiling {n} chunk(s)");
        let batch = Rc::new(RefCell::new(Batch {
            results: vec![None; n],
            done: 0,
        }));
        for (i, json) in chunks.into_iter().enumerate() {
            let ident = NSString::from_str(&format!("{}-{i}", set.identifier));
            let json = NSString::from_str(&json);
            let (batch, store_c, app, set) =
                (batch.clone(), store.clone(), app.clone(), set.clone());
            let handler = RcBlock::new(move |list: *mut WKContentRuleList, err: *mut NSError| {
                if list.is_null() && !err.is_null() {
                    let desc = unsafe { (*err).localizedDescription() };
                    crate::dbg_log!("blocklist chunk {i} compile failed: {desc}");
                }
                let mut b = batch.borrow_mut();
                b.results[i] = unsafe { Retained::retain(list) };
                b.done += 1;
                if b.done < n {
                    return;
                }
                let results = std::mem::take(&mut b.results);
                drop(b);
                if results.iter().all(Option::is_some) {
                    crate::dbg_log!("blocklist compiled ({n} chunk(s))");
                    let lists = set
                        .categories
                        .iter()
                        .cloned()
                        .zip(results.into_iter().flatten())
                        .collect();
                    install(&app, lists, on_ready);
                } else {
                    compile_fallback(store_c.clone(), app.clone(), set.clone(), on_ready);
                }
            });
            unsafe {
                store.compileContentRuleListForIdentifier_encodedContentRuleList_completionHandler(
                    Some(&ident),
                    Some(&json),
                    Some(&handler),
                );
            }
        }
    }

    /// Remove compiled lists left over from previous rule-set versions.
    fn prune_stale(
        store: Retained<WKContentRuleListStore>,
        keep_prefix: String,
        keep_fallback: String,
    ) {
        let store2 = store.clone();
        let handler = RcBlock::new(move |ids: *mut objc2_foundation::NSArray<NSString>| {
            if ids.is_null() {
                return;
            }
            // SAFETY: WebKit passes a valid array on the main thread.
            let ids = unsafe { &*ids };
            for id in ids.iter() {
                let s = id.to_string();
                let ours = s.starts_with("foxlite-");
                let current = s.starts_with(&keep_prefix) || s == keep_fallback;
                if ours && !current {
                    crate::dbg_log!("removing stale compiled blocklist {s}");
                    let noop = RcBlock::new(|_err: *mut NSError| {});
                    unsafe {
                        store2.removeContentRuleListForIdentifier_completionHandler(
                            Some(&id),
                            Some(&noop),
                        );
                    }
                }
            }
        });
        unsafe { store.getAvailableContentRuleListIdentifiers(Some(&handler)) };
    }

    /// Make the blocker rules available (async inside WebKit) and, once ready,
    /// attach them to every existing tab. WebKit caches compiled lists on disk
    /// by identifier, so we look every chunk up first and only compile when
    /// this build's rule set has never been compiled. `on_ready` runs on the
    /// main thread after the lists are available.
    pub fn blocker_init(app: &AppHandle, set: RuleSet, on_ready: fn(&AppHandle)) {
        let Some(mtm) = MainThreadMarker::new() else {
            BLOCKER.with(|b| b.set(BlockerStatus::Failed));
            return;
        };
        let Some(store) = (unsafe { WKContentRuleListStore::defaultStore(mtm) }) else {
            BLOCKER.with(|b| b.set(BlockerStatus::Failed));
            return;
        };
        let set = Rc::new(set);
        prune_stale(
            store.clone(),
            format!("{}-", set.identifier),
            set.fallback_identifier.clone(),
        );

        let n = set.categories.len();
        if n == 0 {
            compile_fallback(store, app.clone(), set, on_ready);
            return;
        }
        let batch = Rc::new(RefCell::new(Batch {
            results: vec![None; n],
            done: 0,
        }));
        for i in 0..n {
            let ident = NSString::from_str(&format!("{}-{i}", set.identifier));
            let (batch, store_c, app, set) =
                (batch.clone(), store.clone(), app.clone(), set.clone());
            let handler = RcBlock::new(move |list: *mut WKContentRuleList, _err: *mut NSError| {
                let mut b = batch.borrow_mut();
                b.results[i] = unsafe { Retained::retain(list) };
                b.done += 1;
                if b.done < n {
                    return;
                }
                let results = std::mem::take(&mut b.results);
                drop(b);
                if results.iter().all(Option::is_some) {
                    crate::dbg_log!("blocklist loaded from WebKit cache ({n} chunk(s))");
                    let lists = set
                        .categories
                        .iter()
                        .cloned()
                        .zip(results.into_iter().flatten())
                        .collect();
                    install(&app, lists, on_ready);
                } else {
                    compile_all(store_c.clone(), app.clone(), set.clone(), on_ready);
                }
            });
            unsafe {
                store.lookUpContentRuleListForIdentifier_completionHandler(
                    Some(&ident),
                    Some(&handler),
                );
            }
        }
    }

    pub fn blocker_status() -> BlockerStatus {
        BLOCKER.with(|b| b.get())
    }

    /// Attach exactly the compiled lists whose category is in `enabled`
    /// (detaching everything else). Applies to the webview's next loads.
    pub fn blocker_apply(webview: &Webview, enabled: std::collections::HashSet<String>) {
        with_view(webview, move |view| unsafe {
            let ucc = view.configuration().userContentController();
            ucc.removeAllContentRuleLists();
            RULES.with(|r| {
                for (category, list) in r.borrow().iter() {
                    if enabled.contains(category) {
                        ucc.addContentRuleList(list);
                    }
                }
            });
        });
    }

    // ---- page policies (WKPreferences) --------------------------------------

    /// Pop-up blocking + fraudulent-site warnings, applied per webview.
    ///
    /// - `block_popups`: WebKit's own pop-up blocker — `window.open` without a
    ///   user gesture is silently refused (user-initiated opens still reach our
    ///   new-window handler and become tabs), like Safari.
    /// - `fraud_warnings`: WebKit's Safe Browsing check (Google Safe Browsing
    ///   via Apple's service) shows an interstitial on known phishing/malware
    ///   pages before they load.
    pub fn set_page_policies(webview: &Webview, block_popups: bool, fraud_warnings: bool) {
        with_view(webview, move |view| unsafe {
            let prefs = view.configuration().preferences();
            prefs.setJavaScriptCanOpenWindowsAutomatically(!block_popups);
            prefs.setFraudulentWebsiteWarningEnabled(fraud_warnings);
        });
    }

    /// Debug/self-test only: is WebKit's Safe Browsing interstitial showing?
    #[cfg(debug_assertions)]
    pub fn safe_browsing_warning_shown(webview: &Webview) -> bool {
        let out = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let o = out.clone();
        with_view(webview, move |view| {
            let sel: Sel = sel!(_safeBrowsingWarning);
            if view.respondsToSelector(sel) {
                let w: *mut AnyObject = unsafe { msg_send![view, _safeBrowsingWarning] };
                o.store(!w.is_null(), std::sync::atomic::Ordering::SeqCst);
            }
        });
        out.load(std::sync::atomic::Ordering::SeqCst)
    }

    // ---- system preferences -------------------------------------------------

    /// The user's "Reduce transparency" accessibility setting.
    pub fn system_reduce_transparency() -> bool {
        unsafe {
            let ws: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
            if ws.is_null() {
                return false;
            }
            msg_send![ws, accessibilityDisplayShouldReduceTransparency]
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::{BlockerStatus, KeyHandler, NavHandler};
    use tauri::{AppHandle, Webview};

    pub fn observe_nav(_webview: &Webview, _handler: NavHandler) {}
    pub fn bring_to_front(_webview: &Webview) {}
    pub fn install_key_monitor(_handler: KeyHandler) {}
    pub fn forget(_label: &str) {}
    pub fn web_process_pid(_webview: &Webview) -> Option<i32> {
        None
    }
    pub struct PageHandle;
    pub fn retain_page(_webview: &Webview) -> Option<PageHandle> {
        None
    }
    impl PageHandle {
        pub fn terminate_process(&self) -> bool {
            false
        }
        pub fn close(&self) {}
    }
    pub fn phys_footprint(_pid: i32) -> Option<u64> {
        None
    }
    pub fn own_pid() -> i32 {
        std::process::id() as i32
    }
    pub fn go_back(_webview: &Webview) -> bool {
        false
    }
    pub fn go_forward(_webview: &Webview) -> bool {
        false
    }
    pub fn stop_loading(_webview: &Webview) -> bool {
        false
    }
    pub fn reload_from_origin(_webview: &Webview) -> bool {
        false
    }
    pub fn set_user_agent(_webview: &Webview, _ua: String) {}
    pub fn blocker_init(
        _app: &AppHandle,
        _set: crate::blocklist::RuleSet,
        _on_ready: fn(&AppHandle),
    ) {
    }
    pub fn blocker_status() -> BlockerStatus {
        BlockerStatus::Unsupported
    }
    pub fn blocker_apply(_webview: &Webview, _enabled: std::collections::HashSet<String>) {}
    pub fn set_page_policies(_webview: &Webview, _block_popups: bool, _fraud_warnings: bool) {}
    #[cfg(debug_assertions)]
    pub fn safe_browsing_warning_shown(_webview: &Webview) -> bool {
        false
    }
    pub fn system_reduce_transparency() -> bool {
        false
    }
}

pub use imp::*;
