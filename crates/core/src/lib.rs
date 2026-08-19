//! Sway's engine: library, file identity and sync.
//!
//! This is where everything that doesn't need a screen lives. The Tauri app
//! (`app/src-tauri`) and the file server (Phase 6.2) are two fronts on top
//! of this same crate, and that's the reason it exists: the server runs
//! headless on an Ubuntu box without a desktop and can't compile `tauri`.
//!
//! What does NOT belong here: pairing with human confirmation, mDNS
//! discovery, iTunes XML export, playback. That stays on the app side.
//!
//! The sync entry point is `engine::Host`: whoever uses this crate tells
//! the engine how to reach the database, where the files are, and what to
//! do when something changes.

/// Re-exported so that whoever uses the crate doesn't have to declare
/// `rusqlite` on their own: two different versions of the same library in
/// the same binary are two incompatible `Connection` types, and the error
/// you get looks nothing like the cause.
pub use rusqlite;

/// Where timings are dropped, in addition to the log. Set by whoever uses
/// the crate (in the app, Tauri's `setup`).
static PERF_FILE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// TEMPORARY — diagnostics.
///
/// On Android `log::*` goes to logcat, but some devices ship it disabled at
/// the system level (`live.logcat=disable`, which not even the adb shell can
/// change): there the whole buffer returns zero lines and there's no way to
/// see a timing. A file next to the DB can be pulled out with `run-as`
/// without touching any phone setting.
pub fn perf_line(line: &str) {
    use std::io::Write;
    let Some(path) = PERF_FILE.get() else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{} {line}", db::now_ms());
    }
}

/// Where to write the timings. Without this, `perf_line` does nothing.
pub fn set_perf_file(path: std::path::PathBuf) {
    let _ = PERF_FILE.set(path);
}

pub mod db;
pub mod device_info;
pub mod engine;
pub mod hashing;
pub mod id3_sanitize;
pub mod import;
pub mod manifest;
pub mod merge;
pub mod pairing;
pub mod rank;
pub mod scope;
pub mod transfer;
pub mod trash;
pub mod wire;
