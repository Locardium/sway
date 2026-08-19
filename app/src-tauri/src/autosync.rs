//! Automatic synchronization.
//!
//! A sync that has to be requested by hand doesn't work for the real use
//! case: you import a song on the PC and want to find it on the phone
//! without having to remember to do anything.
//!
//! It's triggered for three reasons, each one covering a gap left by the
//! previous one:
//!
//! - **Something changed here** (import, edited playlist, moved track). With
//!   a few seconds of breathing room: importing a folder is hundreds of
//!   changes in a row, and syncing on every single one would be absurd.
//! - **A device showed up**. The phone was off wifi all afternoon; when it
//!   comes back it has to catch up without waiting for something else to
//!   change.
//! - **Every so often**, as a safety net in case either of the two above got
//!   missed.
//!
//! The infinite loop cuts itself off: applying changes from the other side
//! triggers its own "something changed", but that sync no longer finds
//! anything to do and ends without triggering anything again.

use crate::AppState;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tauri::{AppHandle, Manager};

/// How long to wait since the last change before syncing.
///
/// Half a second is enough: large operations already come in as ONE command
/// (importing a folder, dragging twenty tracks into a playlist), so this
/// only coalesces separate commands fired almost together. And the cost of
/// an extra sync is cheap — a manifest that comes back with nothing to do —
/// while the wait itself is noticeable.
const QUIET_PERIOD_MS: i64 = 500;
/// Safety net.
const PERIODIC: Duration = Duration::from_secs(10 * 60);
/// How long the local network gets before syncing with the server anyway.
///
/// It's a ceiling, not a wait: as soon as the LAN finishes, it moves on. It
/// exists so a large transfer — or a peer that fell asleep halfway through —
/// doesn't leave the server unsynced for the rest of the day.
pub(crate) const LAN_FIRST_MAX_WAIT: Duration = Duration::from_secs(15 * 60);
/// Added to the breathing room, this is the ceiling on how long it takes a
/// sync to start.
const TICK: Duration = Duration::from_millis(250);
/// How often to retry when there's a change to propagate but no device is
/// available.
///
/// Without this, the pending flag **isn't cleared** (on purpose: a change
/// made with the phone off has to travel when it comes back), so the loop
/// would consider itself awake on every tick: four times a second it would
/// take the DB lock to list devices, write a log line, and start over — for
/// nothing. Against the same lock the UI needs. And if the peer was marked
/// offline, the change would only go out on the 10-minute safety net.
const RETRY_WHEN_ALONE: Duration = Duration::from_secs(15);

#[derive(Default)]
pub struct AutoSync {
    /// Timestamp of the last unsynced local change. 0 = nothing pending.
    pending_since: AtomicI64,
}

/// Records in the DB that there's something local not yet propagated.
///
/// The pending flag used to live only in memory, and that was enough as long
/// as every startup began by comparing the two whole libraries. Since that no
/// longer happens (see `watch.rs`), a change made with no one listening and
/// the app closed afterward had no one to push it: the other side doesn't
/// know it exists, so it never asks, and this side kept no trace that it
/// still needed to be sent.
const PENDING: &str = "autosync_pending";

impl AutoSync {
    /// Called by anything that modifies the library. Returns `true` if there
    /// was nothing pending until just now — i.e. if it needs to be recorded.
    pub fn note_change(&self) -> bool {
        let before = self
            .pending_since
            .swap(crate::db::now_ms(), Ordering::Relaxed);
        before == 0
    }

    #[cfg(test)]
    fn pending(&self) -> i64 {
        self.pending_since.load(Ordering::Relaxed)
    }

    /// There's a pending change and the quiet period has already passed. It
    /// doesn't consume it: it's only cleared once it could be sent to
    /// someone, so a change made with the phone off isn't lost.
    fn is_settled(&self) -> bool {
        let since = self.pending_since.load(Ordering::Relaxed);
        since != 0 && crate::db::now_ms() - since >= QUIET_PERIOD_MS
    }

    /// Returns `true` if there really was something, so we don't write to the
    /// DB on every loop iteration.
    fn clear(&self) -> bool {
        self.pending_since.swap(0, Ordering::Relaxed) != 0
    }
}

pub fn enabled(conn: &rusqlite::Connection) -> bool {
    crate::db::get_setting(conn, "auto_sync_p2p")
        .ok()
        .flatten()
        .map(|v| v == "1")
        .unwrap_or(true)
}

pub fn set_enabled(conn: &rusqlite::Connection, on: bool) -> rusqlite::Result<()> {
    crate::db::set_setting(conn, "auto_sync_p2p", if on { "1" } else { "0" })
}

/// Notice that something changed in this library.
pub fn note_change(handle: &AppHandle) {
    log::info!("[autosync] local change noted");
    let state = handle.state::<AppState>();
    // Only on the first of a batch: importing a folder is hundreds of
    // notices in a row, and there's no need to write the same flag hundreds
    // of times.
    if state.autosync.note_change() {
        if let Ok(conn) = state.db.lock() {
            let _ = crate::db::set_setting(&conn, PENDING, "1");
        }
    }
}

/// Whatever was left pending from the previous run.
///
/// Called on startup: a change that didn't make it out has to go out now,
/// and no one else is going to remember it.
fn restore_pending(handle: &AppHandle) {
    let state = handle.state::<AppState>();
    let pending = {
        let Ok(conn) = state.db.lock() else { return };
        crate::db::get_setting(&conn, PENDING)
            .ok()
            .flatten()
            .map(|v| v == "1")
            .unwrap_or(false)
    };
    if pending {
        log::info!("[autosync] there were local changes left from the previous run");
        state.autosync.note_change();
    }
}

/// A device became available again: catch up.
///
/// Called from two places, because neither one covers the other: the TCP
/// probe sees the one that was offline and came back, and discovery sees the
/// one that showed up on the network for the first time (that one is born
/// `online`, so there's no transition to probe). Unpaired devices are
/// filtered out here so neither caller has to remember to.
pub fn peer_came_online(handle: &AppHandle, uid: &str) {
    let state = handle.state::<AppState>();
    let platform: String = {
        let guard = state.db.lock();
        let Ok(conn) = guard else { return };
        if !enabled(&conn) {
            return;
        }
        let found = conn
            .query_row("SELECT platform FROM devices WHERE uid = ?1", [uid], |r| {
                r.get(0)
            })
            .ok();
        // Not paired: there's nothing to sync with it.
        let Some(platform) = found else { return };
        platform
    };
    let remote = platform == sway_core::pairing::PLATFORM_SERVER;
    // A server with a watcher catches up on its own, and better: it asks
    // with the revision it already knew and syncs only if something was
    // really missed. Also launching the sync here would mean a full run
    // against the file server every time it shows up again.
    if remote && state.watchers.is_watched(uid) {
        log::debug!("[autosync] {uid} is being watched: leaving the catch-up to the watcher");
        return;
    }
    {
        let conditions = crate::power::current(&state);
        let guard = state.db.lock();
        let Ok(conn) = guard else { return };
        let limits = crate::power::Limits::load(&conn);
        if let Some(h) = crate::power::hold(&conditions, &limits, remote) {
            log::info!("[autosync] {uid} showed up but sync is on hold: {}", h.reason());
            return;
        }
    }
    log::info!("[autosync] {uid} is available: catching up");

    // A server that shows up waits for the local network, for the same
    // reason as in the loop below: when the app starts, both usually show
    // up at once, and downloading over the LAN what the server won't then
    // have to send is the difference between a second and several minutes
    // of internet.
    if remote {
        let handle = handle.clone();
        let uid = uid.to_string();
        std::thread::spawn(move || {
            let lan = lan_peers(&handle);
            crate::pairing::wait_until_idle(&handle, &lan, LAN_FIRST_MAX_WAIT);
            crate::pairing::sync_files_auto(handle.clone(), uid);
        });
        return;
    }
    crate::pairing::sync_files_auto(handle.clone(), uid.to_string());
}

/// The paired and available devices that aren't a server.
pub(crate) fn lan_peers(handle: &AppHandle) -> Vec<String> {
    let state = handle.state::<AppState>();
    let guard = state.db.lock();
    let Ok(conn) = guard else { return Vec::new() };
    state
        .peers
        .merged_list(&conn)
        .into_iter()
        .filter(|p| p.paired && p.online && p.platform != sway_core::pairing::PLATFORM_SERVER)
        .map(|p| p.uid)
        .collect()
}

pub fn spawn(handle: AppHandle) {
    std::thread::spawn(move || {
        restore_pending(&handle);
        let mut since_periodic = std::time::Instant::now();
        // Before this moment, propagation isn't retried. It only moves
        // forward when an attempt found no one.
        let mut next_try = std::time::Instant::now();
        loop {
            std::thread::sleep(TICK);
            let state = handle.state::<AppState>();

            let due_periodic = since_periodic.elapsed() >= PERIODIC;
            let due_change = state.autosync.is_settled() && std::time::Instant::now() >= next_try;
            if !due_periodic && !due_change {
                continue;
            }
            if due_periodic {
                since_periodic = std::time::Instant::now();
            }

            let peers: Vec<(String, String)> = {
                let guard = state.db.lock();
                let Ok(conn) = guard else {
                    log::warn!("[autosync] could not take the DB lock");
                    continue;
                };
                if !enabled(&conn) {
                    log::debug!("[autosync] turned off in settings");
                    continue;
                }

                state
                    .peers
                    .merged_list(&conn)
                    .into_iter()
                    .filter(|p| p.paired && p.online)
                    .map(|p| (p.uid, p.platform))
                    .collect()
            };
            if peers.is_empty() {
                // The pending flag isn't cleared: a change made while the
                // other device is off has to travel when it comes back, not
                // get lost here. But it waits before looking again, or this
                // is a busy loop against the DB lock.
                next_try = std::time::Instant::now() + RETRY_WHEN_ALONE;
                // Distinguish the two cases: this is reached both with
                // something pending and on the periodic pass with nothing to
                // do, and always saying "there are changes to propagate"
                // wastes time when reading the log.
                if due_change {
                    log::info!(
                        "[autosync] there are changes to propagate but no paired device is available"
                    );
                } else {
                    log::debug!("[autosync] no paired device available");
                }
                // It could be marked offline from a stale probe: ask again
                // now instead of waiting for the safety net.
                let h = handle.clone();
                std::thread::spawn(move || crate::discovery::probe_once(&h));
                continue;
            }
            next_try = std::time::Instant::now();
            if due_change && state.autosync.clear() {
                if let Ok(conn) = state.db.lock() {
                    let _ = crate::db::set_setting(&conn, PENDING, "0");
                }
                log::info!("[autosync] local changes -> {} device(s)", peers.len());
            }

            // Network and battery. An automatic sync can wait; one requested
            // by hand doesn't go through here, and it's the fallback for
            // when the OS gets the network wrong.
            let (conditions, limits) = {
                let now = crate::power::current(&state);
                let guard = state.db.lock();
                let Ok(conn) = guard else { continue };
                (now, crate::power::Limits::load(&conn))
            };

            // The local network first, the server after.
            //
            // It's not "one or the other": the server needs the bytes too,
            // it's the file server. It's about the order. The LAN moves a
            // track in a second and without spending internet; when it's
            // later the server's turn, the inventory already counts what
            // just arrived and asks only for what's really missing. The
            // other way around, the same thing would download twice, the
            // first time over the slow path.
            let (servers, lan): (Vec<_>, Vec<_>) = peers
                .into_iter()
                .partition(|(_, platform)| platform == sway_core::pairing::PLATFORM_SERVER);
            let mut servers: Vec<String> = servers.into_iter().map(|(uid, _)| uid).collect();
            let lan: Vec<String> = lan.into_iter().map(|(uid, _)| uid).collect();

            // The safety net doesn't cover a device someone is already
            // watching. With the standing connection open (see `watch.rs`)
            // the server announces anything as it happens, so this pass can
            // only end in "there was nothing to do" — and finding that out
            // costs the whole inventory: with 5000 tracks that's 4 MB every
            // ten minutes, 576 MB a day, per device and over the internet.
            // Only the periodic pass is skipped: a local change has to be
            // pushed regardless of whether someone is watching it or not.
            if !due_change {
                servers.retain(|uid| {
                    let watched = state.watchers.is_live(uid);
                    if watched {
                        log::debug!("[autosync] {uid} is already being watched: skipping the periodic pass");
                    }
                    !watched
                });
            }

            let lan = match crate::power::hold(&conditions, &limits, false) {
                Some(h) => {
                    log::info!("[autosync] local network on hold: {}", h.reason());
                    Vec::new()
                }
                None => lan,
            };
            let servers = match crate::power::hold(&conditions, &limits, true) {
                Some(h) => {
                    log::info!("[autosync] server on hold: {}", h.reason());
                    Vec::new()
                }
                None => servers,
            };

            for uid in lan.iter().cloned() {
                crate::pairing::sync_files_auto(handle.clone(), uid);
            }
            if !servers.is_empty() {
                let handle = handle.clone();
                std::thread::spawn(move || {
                    crate::pairing::wait_until_idle(&handle, &lan, LAN_FIRST_MAX_WAIT);
                    for uid in servers {
                        crate::pairing::sync_files_auto(handle.clone(), uid);
                    }
                });
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_waits_for_the_quiet_period() {
        let a = AutoSync::default();
        assert!(!a.is_settled(), "no changes, nothing to do");

        assert!(a.note_change(), "the first one has to be recorded");
        assert!(!a.note_change(), "the second one is already recorded");
        assert!(!a.is_settled(), "just changed: has to wait");

        // Simulates that the quiet period has passed.
        a.pending_since
            .store(crate::db::now_ms() - QUIET_PERIOD_MS - 1, Ordering::Relaxed);
        assert!(a.is_settled());
    }

    /// Importing a folder is hundreds of changes in a row: each one restarts
    /// the wait so only one sync fires at the end.
    #[test]
    fn a_new_change_restarts_the_wait() {
        let a = AutoSync::default();
        a.pending_since
            .store(crate::db::now_ms() - QUIET_PERIOD_MS - 1, Ordering::Relaxed);
        assert!(a.is_settled());
        a.note_change();
        assert!(!a.is_settled());
    }

    /// Checking the pending change doesn't consume it: if there's no one to
    /// send it to, it has to stay pending until a device shows up.
    #[test]
    fn checking_does_not_consume_the_pending_change() {
        let a = AutoSync::default();
        a.pending_since
            .store(crate::db::now_ms() - QUIET_PERIOD_MS - 1, Ordering::Relaxed);
        assert!(a.is_settled());
        assert!(a.is_settled(), "checking it twice doesn't clear it");
        assert_ne!(a.pending(), 0);
        assert!(a.clear(), "there was something to clear");
        assert!(!a.clear(), "and clearing again doesn't write again");
        assert!(!a.is_settled());
    }
}
