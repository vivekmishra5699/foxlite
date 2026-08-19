//! Scripted smoke test (debug builds only): `FOXLITE_SELFTEST=1 foxlite` walks
//! through the tab lifecycle on real sites — open, navigate, close the active
//! tab, discard/revive, close a background tab — logging each step, then quits.
//! Lets us catch hangs/crashes without driving the UI by hand: if a step never
//! logs "done", `sample` the process.

use std::time::Duration;

use tauri::{AppHandle, Emitter};

use crate::{state, tabs};

/// Ask the chrome UI to invoke `cmd` — so the command runs exactly as a click
/// would: inside WebKit's IPC callout on the main thread.
fn via_chrome(app: &AppHandle, cmd: &str, args: serde_json::Value) {
    let _ = app.emit_to(
        "chrome",
        "selftest",
        serde_json::json!({ "cmd": cmd, "args": args }),
    );
}

struct Step {
    at_ms: u64,
    name: &'static str,
    run: fn(&AppHandle),
}

const STEPS: &[Step] = &[
    // Like a user: type a search into the home tab's address bar (same-webview
    // navigation from an internal page to the web).
    Step {
        at_ms: 2500,
        name: "search from home tab via chrome IPC",
        run: |app| {
            via_chrome(
                app,
                "navigate",
                serde_json::json!({ "input": "foxlite browser" }),
            );
        },
    },
    Step {
        at_ms: 5000,
        name: "open google search",
        run: |app| {
            tabs::open_external(
                app,
                "https://www.google.com/search?q=foxlite+browser"
                    .parse()
                    .unwrap(),
            );
        },
    },
    Step {
        at_ms: 7000,
        name: "open example.com",
        run: |app| {
            tabs::open_external(app, "https://example.com/".parse().unwrap());
        },
    },
    // Blocker check: known ad/tracker scripts must fail to load, a control
    // script must load.
    Step {
        at_ms: 9000,
        name: "probe ad/tracker loads",
        run: |app| {
            tabs::run_js_active(
                app,
                r#"(function(){
          window.__probe = {};
          function probe(name, url){ var s=document.createElement('script'); s.src=url;
            s.onload=function(){window.__probe[name]='LOADED'}; s.onerror=function(){window.__probe[name]='blocked'};
            document.head.appendChild(s); }
          probe('control_jquery','https://code.jquery.com/jquery-3.7.1.min.js');
          probe('adsbygoogle','https://pagead2.googlesyndication.com/pagead/js/adsbygoogle.js');
          probe('gtm','https://www.googletagmanager.com/gtm.js?id=GTM-XXXX');
          probe('doubleclick','https://securepubads.g.doubleclick.net/tag/js/gpt.js');
          probe('facebook_pixel','https://connect.facebook.net/en_US/fbevents.js');
        })()"#,
            );
        },
    },
    Step {
        at_ms: 12000,
        name: "read blocker probe",
        run: |app| {
            let label = state::lock(app)
                .active_tab()
                .map(|t| t.label())
                .unwrap_or_default();
            if let Some(v) = tauri::Manager::get_webview(app, &label) {
                let _ = v.eval_with_callback("JSON.stringify(window.__probe)", |r| {
                    crate::dbg_log!("selftest: blocker probe {r}");
                });
            }
        },
    },
    // Pop-up blocker: a page that calls `window.open` on load (no user
    // gesture) must not open a tab. (Scripts run via evaluateJavaScript carry
    // a synthetic user gesture in WebKit, so the page has to do it itself.)
    Step {
        at_ms: 13000,
        name: "load a page that pops on load (should be blocked)",
        run: |app| {
            tabs::navigate_active(app, "data:text/html,<script>window.open('https://example.org/','_blank')</script><p>popup test</p>".parse().unwrap());
        },
    },
    Step {
        at_ms: 15000,
        name: "count tabs after popup attempt (expect 3)",
        run: |app| {
            let n = state::lock(app).tabs.len();
            crate::dbg_log!(
                "selftest: popup check -> {n} tabs ({})",
                if n == 3 { "BLOCKED ok" } else { "NOT BLOCKED" }
            );
        },
    },
    // Fraud warning: WebKit's Safe Browsing interstitial on Google's test page.
    Step {
        at_ms: 15500,
        name: "open safe-browsing test page",
        run: |app| {
            tabs::navigate_active(
                app,
                "https://testsafebrowsing.appspot.com/s/phishing.html"
                    .parse()
                    .unwrap(),
            );
        },
    },
    Step {
        at_ms: 22000,
        name: "check fraud warning",
        run: |app| {
            let label = state::lock(app)
                .active_tab()
                .map(|t| t.label())
                .unwrap_or_default();
            let shown = tauri::Manager::get_webview(app, &label)
                .is_some_and(|v| crate::native::safe_browsing_warning_shown(&v));
            crate::dbg_log!("selftest: safe browsing warning shown = {shown}");
            tabs::navigate_active(app, "https://example.com/".parse().unwrap());
        },
    },
    // Private tabs: isolated session, wiped on close, pop-ups stay private.
    Step {
        at_ms: 25500,
        name: "open private tab on example.com",
        run: |app| {
            tabs::open_external_as(app, "https://example.com/".parse().unwrap(), true);
        },
    },
    Step {
        at_ms: 28500,
        name: "set a cookie in the private tab + open a link from it",
        run: |app| {
            tabs::run_js_active(app, "document.cookie='foxpriv=1; path=/'; window.open('https://example.org/','_blank');");
        },
    },
    Step {
        at_ms: 30500,
        name: "check the opened tab is private, then close both",
        run: |app| {
            let (n_priv, ids): (usize, Vec<usize>) = {
                let st = state::lock(app);
                (
                    st.tabs.iter().filter(|t| t.incognito).count(),
                    st.tabs
                        .iter()
                        .filter(|t| t.incognito)
                        .map(|t| t.id)
                        .collect(),
                )
            };
            crate::dbg_log!(
                "selftest: private tabs open = {n_priv} ({})",
                if n_priv == 2 {
                    "link stayed private ok"
                } else {
                    "LEAKED to normal tab"
                }
            );
            for id in ids {
                tabs::close_tab(app, id);
            }
        },
    },
    Step {
        at_ms: 32500,
        name: "new private tab: cookie must be gone",
        run: |app| {
            tabs::open_external_as(app, "https://example.com/".parse().unwrap(), true);
        },
    },
    Step {
        at_ms: 35500,
        name: "read cookie in fresh private tab",
        run: |app| {
            let label = state::lock(app)
                .active_tab()
                .map(|t| t.label())
                .unwrap_or_default();
            if let Some(v) = tauri::Manager::get_webview(app, &label) {
                let _ = v.eval_with_callback("document.cookie", |r| {
                    crate::dbg_log!(
                        "selftest: fresh private tab cookie = {r} ({})",
                        if r.contains("foxpriv") {
                            "LEAKED"
                        } else {
                            "isolated ok"
                        }
                    );
                });
            }
        },
    },
    Step {
        at_ms: 36500,
        name: "close private tab",
        run: tabs::close_active,
    },
    Step {
        at_ms: 37500,
        name: "focus google's search box in the page",
        run: |app| {
            // Make Foxlite the key window with the page as first responder, like a
            // user clicking into the page.
            if let Some(w) = tauri::Manager::get_window(app, "main") {
                let _ = w.set_focus();
            }
            tabs::select_index(app, 1, false);
            tabs::run_js_active(app, "(function(){var f=document.querySelector('textarea[name=q],input[name=q]');if(f){f.focus();f.value='hello';f.dispatchEvent(new Event('input',{bubbles:true}));}})()");
        },
    },
    Step {
        at_ms: 39500,
        name: "close active (google, focused field) via chrome IPC",
        run: |app| {
            let id = state::lock(app).active_tab().map(|t| t.id).unwrap_or(0);
            via_chrome(app, "close_tab", serde_json::json!({ "id": id }));
        },
    },
    Step {
        at_ms: 41500,
        name: "close active (example.com) via chrome IPC",
        run: |app| {
            let id = state::lock(app).active_tab().map(|t| t.id).unwrap_or(0);
            via_chrome(app, "close_tab", serde_json::json!({ "id": id }));
        },
    },
    Step {
        at_ms: 42000,
        name: "open wikipedia",
        run: |app| {
            tabs::open_external(
                app,
                "https://en.wikipedia.org/wiki/Web_browser".parse().unwrap(),
            );
        },
    },
    Step {
        at_ms: 46000,
        name: "open home (backgrounds wikipedia)",
        run: |app| tabs::open_home(app, false),
    },
    Step {
        at_ms: 47500,
        name: "sleep background tabs",
        run: tabs::discard_all_background,
    },
    Step {
        at_ms: 49000,
        name: "revive wikipedia (select index 1)",
        run: |app| tabs::select_index(app, 1, false),
    },
    Step {
        at_ms: 53000,
        name: "close background home tab",
        run: |app| {
            let id = state::lock(app)
                .tabs
                .iter()
                .find(|t| t.is_internal())
                .map(|t| t.id);
            if let Some(id) = id {
                tabs::close_tab(app, id);
            }
        },
    },
    Step {
        at_ms: 55000,
        name: "close active (wikipedia) via chrome IPC",
        run: |app| {
            let id = state::lock(app).active_tab().map(|t| t.id).unwrap_or(0);
            via_chrome(app, "close_tab", serde_json::json!({ "id": id }));
        },
    },
    Step {
        at_ms: 56500,
        name: "close remaining (searched) tab via chrome IPC",
        run: |app| {
            let id = state::lock(app).active_tab().map(|t| t.id).unwrap_or(0);
            via_chrome(app, "close_tab", serde_json::json!({ "id": id }));
        },
    },
    Step {
        at_ms: 59000,
        name: "quit",
        run: |app| {
            crate::dbg_log!("SELFTEST PASSED");
            app.exit(0);
        },
    },
];

/// Run `js` in the chrome webview and log the (JSON) result under `what`.
fn probe_chrome(app: &AppHandle, what: &'static str, js: &str) {
    if let Some(v) = tauri::Manager::get_webview(app, "chrome") {
        let _ = v.eval_with_callback(js, move |r| {
            crate::dbg_log!("selftest: {what} = {r}");
        });
    }
}

/// Log the logical frames of the chrome and the active tab's webview.
fn log_frames(app: &AppHandle, what: &'static str) {
    let scale = tauri::Manager::get_window(app, "main")
        .and_then(|w| w.scale_factor().ok())
        .unwrap_or(1.0);
    let mut out = String::new();
    let label = state::lock(app)
        .active_tab()
        .map(|t| t.label())
        .unwrap_or_default();
    for l in ["chrome", label.as_str()] {
        if let Some(v) = tauri::Manager::get_webview(app, l) {
            if let (Ok(p), Ok(s)) = (v.position(), v.size()) {
                out.push_str(&format!(
                    " {l}: ({:.0},{:.0}) {:.0}x{:.0}",
                    p.x as f64 / scale,
                    p.y as f64 / scale,
                    s.width as f64 / scale,
                    s.height as f64 / scale
                ));
            }
        }
    }
    crate::dbg_log!("selftest: frames {what}:{out}");
}

/// `FOXLITE_SELFTEST=ui`: the chrome-level features — ⌃⇥ switcher, address
/// bar suggestions (incl. the over-the-page overlay), vertical tabs.
const UI_STEPS: &[Step] = &[
    Step {
        at_ms: 2500,
        name: "open example.com",
        run: |app| tabs::open_external(app, "https://example.com/".parse().unwrap()),
    },
    Step {
        at_ms: 4500,
        name: "open wikipedia",
        run: |app| {
            tabs::open_external(
                app,
                "https://en.wikipedia.org/wiki/Web_browser".parse().unwrap(),
            )
        },
    },
    Step {
        at_ms: 8000,
        name: "switcher: open (⌃⇥)",
        run: |app| {
            crate::dbg_log!("selftest: mru order = {:?}", state::lock(app).mru_order());
            tabs::switcher_step(app, 1);
        },
    },
    Step {
        at_ms: 8700,
        name: "switcher: probe (expect 3 cards, previous tab selected)",
        run: |app| {
            log_frames(app, "switcher open (chrome should cover the window)");
            probe_chrome(
                app,
                "switcher cards:selected-id",
                "document.querySelectorAll('#switcher.show .sw-card').length + ':' + (document.querySelector('.sw-card.sel')||{}).dataset?.id",
            );
        },
    },
    Step {
        at_ms: 9200,
        name: "switcher: step again (⌃⇥)",
        run: |app| tabs::switcher_step(app, 1),
    },
    Step {
        at_ms: 9700,
        name: "switcher: commit (⌃ released) → least recent tab (home)",
        run: |app| {
            tabs::switcher_commit(app);
        },
    },
    Step {
        at_ms: 10500,
        name: "switcher: probe closed + active tab",
        run: |app| {
            let st = state::lock(app);
            crate::dbg_log!(
                "selftest: after commit active = {} ({}), switcher open = {}",
                st.active,
                st.active_tab().map(|t| t.url.clone()).unwrap_or_default(),
                st.switcher.is_some()
            );
            drop(st);
            log_frames(app, "switcher closed (chrome back to strip)");
            probe_chrome(
                app,
                "switcher shown",
                "document.getElementById('switcher').classList.contains('show')",
            );
        },
    },
    Step {
        at_ms: 11500,
        name: "dropdown: focus the address bar",
        run: |app| {
            if let Some(v) = tauri::Manager::get_webview(app, "chrome") {
                let _ = v.eval("document.getElementById('address').focus()");
            }
        },
    },
    Step {
        at_ms: 12500,
        name: "dropdown: probe recent rows + overlay",
        run: |app| {
            crate::dbg_log!(
                "selftest: dropdown_overlay = {:?}",
                state::lock(app).dropdown_overlay
            );
            log_frames(app, "dropdown open (chrome should be taller)");
            probe_chrome(
                app,
                "recent rows",
                "document.querySelectorAll('.suggest.show .sg-row').length",
            );
        },
    },
    Step {
        at_ms: 13000,
        name: "dropdown: type 'wiki'",
        run: |app| {
            if let Some(v) = tauri::Manager::get_webview(app, "chrome") {
                let _ = v.eval("(function(){var a=document.getElementById('address');a.value='wiki';a.dispatchEvent(new InputEvent('input',{inputType:'insertText',data:'i'}));})()");
            }
        },
    },
    Step {
        at_ms: 13800,
        name: "dropdown: probe typed rows",
        run: |app| {
            probe_chrome(
                app,
                "typed rows (first title, count)",
                "(document.querySelector('.suggest.show .sg-row .sg-title')||{}).textContent + ':' + document.querySelectorAll('.suggest.show .sg-row').length",
            );
        },
    },
    Step {
        at_ms: 14300,
        name: "dropdown: ↓",
        run: |app| {
            if let Some(v) = tauri::Manager::get_webview(app, "chrome") {
                let _ = v.eval("(function(){var a=document.getElementById('address');a.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowDown',bubbles:true}));})()");
            }
        },
    },
    Step {
        at_ms: 14600,
        name: "dropdown: probe after ↓",
        run: |app| {
            probe_chrome(
                app,
                "after ↓: value | sel title | rows",
                "document.getElementById('address').value + ' | ' + (document.querySelector('.sg-row.sel .sg-title')||{}).textContent + ' | ' + document.querySelectorAll('.suggest.show .sg-row').length",
            );
        },
    },
    Step {
        at_ms: 14900,
        name: "dropdown: ↵ (open the highlighted row in this tab)",
        run: |app| {
            if let Some(v) = tauri::Manager::get_webview(app, "chrome") {
                let _ = v.eval("(function(){var a=document.getElementById('address');a.dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',bubbles:true}));})()");
            }
        },
    },
    Step {
        at_ms: 16500,
        name: "dropdown: probe closed + navigated",
        run: |app| {
            let st = state::lock(app);
            crate::dbg_log!(
                "selftest: active url = {} , dropdown_overlay = {:?}",
                st.active_tab().map(|t| t.url.clone()).unwrap_or_default(),
                st.dropdown_overlay
            );
            drop(st);
            log_frames(app, "dropdown closed");
        },
    },
    Step {
        at_ms: 17000,
        name: "dropdown: no-match text 'zzqq' then ↓ ↵ (expect a web search)",
        run: |app| {
            if let Some(v) = tauri::Manager::get_webview(app, "chrome") {
                let _ = v.eval("(function(){var a=document.getElementById('address');a.focus();a.value='zzqq';a.dispatchEvent(new InputEvent('input',{inputType:'insertText',data:'q'}));setTimeout(function(){a.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowDown',bubbles:true}));window.__after=a.value+' | rows '+document.querySelectorAll('.suggest.show .sg-row').length;a.dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',bubbles:true}));},400);})()");
            }
        },
    },
    Step {
        at_ms: 19000,
        name: "dropdown: probe no-match navigation",
        run: |app| {
            crate::dbg_log!(
                "selftest: active url = {}",
                state::lock(app)
                    .active_tab()
                    .map(|t| t.url.clone())
                    .unwrap_or_default()
            );
            probe_chrome(app, "after ↓ (no match)", "window.__after");
        },
    },
    Step {
        at_ms: 19500,
        name: "vertical tabs: on",
        run: |app| crate::commands::toggle_vertical_tabs(app.clone()),
    },
    Step {
        at_ms: 20500,
        name: "vertical tabs: probe",
        run: |app| {
            log_frames(app, "vertical (chrome 256 wide, page at x=264)");
            probe_chrome(
                app,
                "vertical class, #chrome size, tabs, toolbar h, address w, tab w×h, tabbar h, bookmarks h",
                "[document.body.classList.contains('vertical'), document.getElementById('chrome').offsetWidth + 'x' + document.getElementById('chrome').offsetHeight, document.querySelectorAll('.tab').length, document.getElementById('toolbar').offsetHeight, document.getElementById('address').offsetWidth, document.querySelector('.tab').offsetWidth + 'x' + document.querySelector('.tab').offsetHeight, document.getElementById('tabbar').offsetHeight, document.getElementById('bookmarks').offsetHeight].join(' ')",
            );
        },
    },
    Step {
        at_ms: 21500,
        name: "vertical tabs: dropdown overlay in sidebar mode",
        run: |app| {
            if let Some(v) = tauri::Manager::get_webview(app, "chrome") {
                let _ = v.eval("document.getElementById('address').focus()");
            }
        },
    },
    Step {
        at_ms: 22500,
        name: "vertical tabs: probe dropdown overlay",
        run: |app| {
            crate::dbg_log!(
                "selftest: dropdown_overlay = {:?}",
                state::lock(app).dropdown_overlay
            );
            log_frames(app, "vertical + dropdown (chrome wider than 256)");
            if let Some(v) = tauri::Manager::get_webview(app, "chrome") {
                let _ = v.eval("document.getElementById('address').blur()");
            }
        },
    },
    Step {
        at_ms: 23500,
        name: "vertical tabs: off",
        run: |app| crate::commands::toggle_vertical_tabs(app.clone()),
    },
    Step {
        at_ms: 24500,
        name: "vertical tabs: probe restored strip",
        run: |app| {
            log_frames(app, "horizontal again (chrome full width, ~88 tall)");
            probe_chrome(
                app,
                "vertical class",
                "document.body.classList.contains('vertical')",
            );
        },
    },
    Step {
        at_ms: 25500,
        name: "quit",
        run: |app| {
            crate::dbg_log!("UI SELFTEST PASSED");
            app.exit(0);
        },
    },
];

pub fn maybe_start(app: &AppHandle) {
    // Watchdog check: freeze the main thread for 75 s and expect a hang-*.txt.
    if std::env::var_os("FOXLITE_SELFTEST_STALL").is_some() {
        let h = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            let _ = h.run_on_main_thread(|| std::thread::sleep(Duration::from_secs(75)));
        });
        return;
    }
    let Some(mode) = std::env::var_os("FOXLITE_SELFTEST") else {
        return;
    };
    let steps: &'static [Step] = if mode == "ui" { UI_STEPS } else { STEPS };
    let app = app.clone();
    std::thread::Builder::new()
        .name("foxlite-selftest".into())
        .spawn(move || {
            let start = std::time::Instant::now();
            for step in steps {
                let at = Duration::from_millis(step.at_ms);
                if let Some(wait) = at.checked_sub(start.elapsed()) {
                    std::thread::sleep(wait);
                }
                let h = app.clone();
                let name = step.name;
                let run = step.run;
                crate::dbg_log!("selftest: {name} …");
                let _ = app.run_on_main_thread(move || {
                    run(&h);
                    let n = state::lock(&h).tabs.len();
                    crate::dbg_log!("selftest: {name} done ({n} tabs)");
                });
            }
        })
        .ok();
}
