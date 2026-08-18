//! Main-thread stall detector. Piggybacks on the housekeeping tick: each tick
//! posts a ping to the main thread and checks whether the previous one was
//! answered. If the main thread hasn't answered for a whole tick the UI is
//! frozen; on macOS we then capture a stack sample of ourselves into the
//! app-data folder (`hang-<unix-ts>.txt`) so a "Foxlite (Not Responding)"
//! report can be diagnosed after the fact. Costs one queued closure per tick.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Manager};

/// Unix seconds when the outstanding ping was posted (0 = none outstanding).
static PING_SENT: AtomicU64 = AtomicU64::new(0);
static REPORTED: AtomicBool = AtomicBool::new(false);

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Call once per housekeeping tick (off the main thread).
pub fn tick(app: &AppHandle) {
    let sent = PING_SENT.load(Ordering::SeqCst);
    if sent != 0 {
        // Previous ping unanswered for a full tick: the main thread is stuck.
        if !REPORTED.swap(true, Ordering::SeqCst) {
            report(app, now().saturating_sub(sent));
        }
        return;
    }
    PING_SENT.store(now(), Ordering::SeqCst);
    let posted = app.run_on_main_thread(|| {
        PING_SENT.store(0, Ordering::SeqCst);
        REPORTED.store(false, Ordering::SeqCst);
    });
    if posted.is_err() {
        PING_SENT.store(0, Ordering::SeqCst);
    }
}

fn report(app: &AppHandle, stalled_for: u64) {
    crate::dbg_log!("main thread unresponsive for ~{stalled_for}s; capturing sample");
    let Ok(dir) = app.path().app_data_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let out = dir.join(format!("hang-{}.txt", now()));
    #[cfg(target_os = "macos")]
    {
        // `sample` is part of macOS; 2 seconds is enough to see the stuck stack.
        let _ = std::process::Command::new("/usr/bin/sample")
            .arg(std::process::id().to_string())
            .arg("2")
            .arg("-file")
            .arg(&out)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = std::fs::write(
            &out,
            format!("main thread unresponsive for ~{stalled_for}s\n"),
        );
    }
}
