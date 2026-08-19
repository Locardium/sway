//! Find out what changed on another device, without asking every so often.
//!
//! Sync is always driven by the caller: the one being asked responds to
//! requests and decides nothing. Between two devices with a screen that's
//! enough, because whoever changes something calls the other and pushes it.
//! Against the file server it's not: changes made away from home reach the
//! server right away, but the server doesn't call anyone —and it can't, which
//! is exactly the point of devices dialing out to it and not the other way
//! around—, so the PC doesn't find out until its next periodic pass. Ten
//! minutes, and up to twenty-five if the local network was still busy.
//!
//! Here who waits gets flipped without flipping who calls: the app opens a
//! connection to the server, sends `Watch` and sits there listening. The
//! server answers `Changed` only when its library moves. The connection goes
//! from the inside out, like all the others, so it still doesn't require
//! anyone to be reachable from the internet.
//!
//! **Why wait instead of asking:** an idle connection costs nothing while
//! nothing happens. Asking every 10 seconds —which is what the reachability
//! poll already did— is 8640 connections per day against the server, and on a
//! phone each one wakes up the radio. While this connection is alive the poll
//! skips that device (see `discovery::probe_once`): the connection itself is
//! proof that it's reachable, and a better one than a connect, because it
//! drops the instant it stops being reachable. The only thing that travels at
//! rest is a heartbeat every 45 seconds.
//!
//! **On a phone this connection drops all the time, and that's fine.** It
//! switches from wifi to data, the carrier's NAT cuts it, Doze kills it with
//! the screen off: measured against the home server, it lived between 17 and
//! 47 seconds at a time. That's why what matters isn't that it lasts, but
//! that **reconnecting be cheap**: the last known revision is sent and the
//! other side answers whether we truly missed something. A drop costs a
//! handshake, not a whole inventory. And that's also why the growing wait
//! only grows when it can't even connect — a drop after a while connected is
//! not a downed server, and treating it as one degrades the instant notice
//! to a five-minute poll right where it's needed most.

use crate::AppState;
use std::collections::HashSet;
use std::sync::{Condvar, Mutex};
use std::time::Duration;
use sway_core::wire::{Mark, Msg};
use tauri::{AppHandle, Emitter, Manager};

/// How long to wait for a heartbeat before declaring the connection dead.
///
/// A bit over two heartbeats (`engine::WATCH_HEARTBEAT` is 45 seconds):
/// missing one alone can't cost a reconnection, but you also can't spend a
/// long while believing there's someone on the other end. It's also the
/// ceiling on how long a downed device can keep showing up green, because
/// while this connection lasts the poll doesn't touch it.
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// Wait after a drop, and its ceiling. Doubles on every failed attempt: a
/// server that's off doesn't deserve an attempt every fifteen seconds all
/// night long.
const RETRY_MIN: Duration = Duration::from_secs(15);
const RETRY_MAX: Duration = Duration::from_secs(5 * 60);

/// What's waited when the connection was running fine and then dropped.
///
/// Short on purpose, and it's what you notice when the server restarts: the
/// connection dies on the spot (the shutdown arrives as a drop, not as
/// silence), so the only thing keeping the device from being up to date
/// again is this wait. With fifteen seconds, restarting the server felt like
/// it "took a while to notice."
///
/// No more is needed: if the drop repeats, the connection still lives for
/// tens of seconds between one and the next, so this isn't a retry loop —
/// it's an occasional handshake. And if it drops on the spot, that's handled
/// by the instant-drop count.
const RETRY_AFTER_DROP: Duration = Duration::from_secs(3);

/// How often it checks whether sync is still on hold (off, metered network,
/// low battery). Not an error: it's a decision that can change.
const HOLD_RETRY: Duration = Duration::from_secs(60);

/// How long to wait when the other side doesn't know how to report changes
/// (a server older than this feature). It's not fixed by retrying; it's just
/// probed again every so often in case it got updated.
const UNSUPPORTED_RETRY: Duration = Duration::from_secs(60 * 60);

/// A wait shorter than this wasn't a wait: the connection opened and dropped
/// on the spot.
///
/// One second, not five. What this needs to recognize is a server that reads
/// the request, doesn't understand it, and disconnects: that happens in less
/// than a round trip, not in seconds. Five seconds was a threshold borrowed
/// from thin air, and everything that fell inside it without being an old
/// server —a network drop right after connecting, a phone handover— was
/// counted as evidence against the other side.
const TOO_QUICK: Duration = Duration::from_secs(1);

/// How many instant drops in a row are enough to stop insisting.
///
/// An old server doesn't answer `Watch`: it accepts the connection, doesn't
/// understand the request, and disconnects. That's not fixed by retrying, so
/// after a few it stops insisting and falls back to the periodic pass.
///
/// **Only counts within the already-open session.** A powered-off server also
/// fails instantly, and that one fixes itself as soon as it's back: counting
/// it here meant an hour of not watching every time the server restarts,
/// which is exactly when you're waiting to see if it's working.
///
/// Five and not three because the cost of the two mistakes changed. Being
/// wrong about the other side costs little: since reconnecting is a
/// handshake and not an inventory, insisting against an old server is cheap.
/// Being wrong about this side costs an hour of watching nothing, silently —
/// which is the kind of failure this file already had twice.
const TOO_QUICK_LIMIT: u32 = 5;

/// How often it checks whether a new server showed up to watch.
const SUPERVISE: Duration = Duration::from_secs(30);

/// How often the whole library is compared even if nobody announced anything.
///
/// Notifications are enough while they work. This is for when they don't: a
/// bug in the revision count, a change that applied only halfway, anything
/// that leaves the two libraries different without anyone finding out.
/// Without this pass, such a divergence never gets fixed — because the
/// notification that should trigger it is exactly the one that failed.
///
/// Six hours and not ten minutes because the comparison isn't free: it's the
/// whole inventory, and with 5000 tracks that's 4 MB. Four times a day is a
/// safety net; every ten minutes was the app's main expense.
const FULL_CHECK: Duration = Duration::from_secs(6 * 60 * 60);

/// Who's being watched, and the bell to wake them up.
///
/// `threads` and `live` are two lists and not one because they answer
/// different questions: the first avoids launching two watchers for the same
/// device and keeps tracking it while it reconnects; the second is only
/// while the connection is actually open, which is what tells the poll not
/// to bother. Confusing them would leave a downed device painted green
/// through the whole backoff.
#[derive(Default)]
pub struct Watchers {
    threads: Mutex<HashSet<String>>,
    live: Mutex<HashSet<String>>,
    /// Pending bells, by uid.
    ///
    /// The server can't announce that it turned on —it doesn't know how to
    /// reach a device behind a NAT, and having them dial out is exactly what
    /// makes this work without opening anything—. But the reachability poll
    /// does see it, every ten seconds. Without this bell that news reached
    /// nobody: the watcher kept sleeping its growing wait, up to five
    /// minutes, against a server that was already up.
    ring: Mutex<HashSet<String>>,
    bell: Condvar,
}

impl Watchers {
    /// Is there an open waiting connection against this device?
    pub fn is_live(&self, uid: &str) -> bool {
        self.live.lock().map(|l| l.contains(uid)).unwrap_or(false)
    }

    /// Is there a watcher in charge of this device, even if it's waiting?
    pub fn is_watched(&self, uid: &str) -> bool {
        self.threads.lock().map(|t| t.contains(uid)).unwrap_or(false)
    }

    /// "Stop waiting, try now." Touched by whoever finds out something the
    /// watcher can't see from where it is: that the server is back, that
    /// network conditions changed.
    pub fn wake(&self, uid: &str) {
        if let Ok(mut ring) = self.ring.lock() {
            ring.insert(uid.to_string());
        }
        // To everyone: each watcher checks whether the bell was for it.
        self.bell.notify_all();
    }

    /// Waits up to `max`, or until the bell rings for this uid.
    ///
    /// A bell that arrived while the watcher was busy isn't lost: it stays
    /// recorded and makes the next wait end immediately.
    fn nap(&self, uid: &str, max: Duration) {
        let Ok(ring) = self.ring.lock() else {
            std::thread::sleep(max);
            return;
        };
        match self.bell.wait_timeout_while(ring, max, |r| !r.contains(uid)) {
            Ok((mut ring, _)) => {
                ring.remove(uid);
            }
            Err(_) => std::thread::sleep(max),
        }
    }
}

/// What to do after a wait ended badly.
#[derive(Debug, PartialEq)]
enum Next {
    /// Retry after this long.
    Retry(Duration),
    /// Stop insisting for a long while: this isn't fixed by retrying.
    StandDown,
}

/// How long to wait before the next attempt, and how many times in a row it
/// dropped instantly.
///
/// Lives apart from the loop so it can be tested. It's not ceremony: the two
/// bugs this ever had lived exactly here —the wait that grew to five minutes
/// and never came back down, and the powered-off server counted as an old
/// server— and neither is visible by reading the code. They're seen by
/// staring at a log twenty minutes later, once you've already eaten the
/// problem.
#[derive(Debug)]
struct Backoff {
    wait: Duration,
    quick_failures: u32,
}

impl Backoff {
    fn new() -> Self {
        Backoff { wait: RETRY_MIN, quick_failures: 0 }
    }

    /// A notification arrived: everything before it stops counting.
    fn news(&mut self) {
        *self = Backoff::new();
    }

    /// Couldn't open the session. The wait grows, but no conclusion is drawn
    /// about the other side: a powered-off server comes back.
    fn unreachable(&mut self) -> Duration {
        let now = self.wait;
        self.wait = (self.wait * 2).min(RETRY_MAX);
        now
    }

    /// The session existed and dropped after `alive`.
    fn dropped(&mut self, alive: Duration) -> Next {
        if alive >= TOO_QUICK {
            // It connected, waited a while, and only then dropped: the other
            // side is fine, what's failing is the network on this end —or
            // the server just restarted—. Retry quickly: letting the wait
            // grow here turns the instant notice into a five-minute poll,
            // right on the device that needs it most and with nothing
            // reporting it.
            *self = Backoff::new();
            return Next::Retry(RETRY_AFTER_DROP);
        }
        self.quick_failures += 1;
        if self.quick_failures >= TOO_QUICK_LIMIT {
            *self = Backoff::new();
            return Next::StandDown;
        }
        let now = self.wait;
        self.wait = (self.wait * 2).min(RETRY_MAX);
        Next::Retry(now)
    }
}

/// What ended the wait.
enum Outcome {
    /// The other side announced there's news.
    Changed,
    /// Speaks the protocol but doesn't know how to announce.
    Unsupported,
    /// Couldn't even open the session: off, no network, restarting.
    ///
    /// Kept separate from the rest because from the outside it looks the
    /// same as a server that drops the connection as soon as it opens —both
    /// fail instantly—, and they mean opposite things: one fixes itself as
    /// soon as it's back, the other never fixes itself. Confusing them left
    /// devices not watching for an hour every time the server restarted.
    Unreachable,
}

/// Keeps one watcher per paired server.
///
/// It's a supervisor thread and not a fixed list because servers get added
/// and removed while the app is open: pairing one has to start watching it
/// without a restart.
pub fn spawn(handle: AppHandle) {
    std::thread::spawn(move || {
        // With no paired server there's nothing to watch, and that looks
        // exactly like "this never started." Saying it once saves looking
        // for the problem in the wrong place.
        let mut announced = false;
        loop {
            let new_ones = servers_to_watch(&handle);
            if !announced {
                announced = true;
                if new_ones.is_empty() {
                    log::info!("[watch] no server paired: nothing to watch");
                }
            }
            for uid in new_ones {
                let handle = handle.clone();
                std::thread::spawn(move || watcher(handle, uid));
            }
            std::thread::sleep(SUPERVISE);
        }
    });
}

/// Paired servers that don't have a watcher yet. Marks them on the spot:
/// between listing them and starting the thread there's a window where the
/// supervisor could pass again and launch a second watcher for the same one.
fn servers_to_watch(handle: &AppHandle) -> Vec<String> {
    let state = handle.state::<AppState>();
    let uids: Vec<String> = {
        let Ok(conn) = state.db.lock() else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare("SELECT uid FROM devices WHERE platform = ?1") else {
            return Vec::new();
        };
        let rows = stmt.query_map([sway_core::pairing::PLATFORM_SERVER], |r| r.get(0));
        match rows {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    };
    let Ok(mut threads) = state.watchers.threads.lock() else {
        return Vec::new();
    };
    uids.into_iter()
        .filter(|uid| threads.insert(uid.clone()))
        .collect()
}

/// A watcher: waits for news, syncs when told, and starts over.
///
/// Lives until the device is no longer paired. A network drop doesn't end
/// it —it reconnects, which is cheap— because ending it would leave the
/// server unwatched until the supervisor noticed.
fn watcher(handle: AppHandle, uid: String) {
    log::info!("[watch] watching {uid} for changes");
    let mut retry = Backoff::new();
    // How much we know about the other side's library. Comes from the
    // database, so it survives the app closing: if nothing changed while we
    // were away, the server says so and nothing gets compared or
    // transferred. `None` = we know nothing about it, so everything has to
    // be compared.
    let mut since: Option<Mark> = stored_mark(&handle, &uid);
    let mut saved = since;
    // When the last full comparison happened.
    let mut last_full = std::time::Instant::now();
    // Last announced pause reason. Without this you'd have to choose between
    // a line per minute or none, and none is worse: a paused watcher looks
    // exactly like one that never started, which is exactly what you're
    // trying to tell apart when sync "isn't working."
    let mut paused: Option<String> = None;
    while still_paired(&handle, &uid) {
        if let Some(reason) = on_hold(&handle) {
            if paused.as_deref() != Some(reason.as_str()) {
                log::info!("[watch] not watching {uid} for now: {reason}");
                paused = Some(reason);
            }
            handle.state::<AppState>().watchers.nap(&uid, HOLD_RETRY);
            continue;
        }
        if let Some(before) = paused.take() {
            log::info!("[watch] watching {uid} again ({before} no longer applies)");
        }

        // Catch up BEFORE waiting, but only the first time: while the app
        // was closed anything could have changed, and just waiting for new
        // news wouldn't bring any of that. From then on the reference is
        // enough — the other side knows if we missed something and answers
        // right away.
        //
        // The difference shows on a phone, where the connection doesn't
        // reach a minute (switches from wifi to data, the carrier's NAT cuts
        // it) and reconnects all the time. Dragging a catch-up into every
        // reconnection would cost more than the periodic pass this was
        // meant to improve on: a whole inventory over the internet every
        // half minute. Every so often everything is compared even if nobody
        // announced anything: notifications are enough while they work, and
        // this is for when they don't.
        if since.is_some() && last_full.elapsed() >= FULL_CHECK {
            log::info!("[watch] full check with {uid}");
            since = None;
        }
        if since.is_none() {
            catch_up(&handle, &uid);
            last_full = std::time::Instant::now();
        }

        let started = std::time::Instant::now();
        let outcome = wait_for_news(&handle, &uid, &mut since);
        // Before looking at how it ended: whatever was learned about the
        // mark counts either way, and especially if it ended badly. It's
        // exactly the connection that dropped that must not make us start
        // over from scratch next time.
        if since != saved {
            remember_mark(&handle, &uid, since);
            saved = since;
        }
        match outcome {
            Ok(Outcome::Changed) => {
                log::info!("[watch] {uid} says there are changes");
                retry.news();
                catch_up(&handle, &uid);
            }
            Ok(Outcome::Unsupported) => {
                log::info!("[watch] {uid} does not report changes; leaving it to the periodic sync");
                since = None;
                nap(&handle, &uid, UNSUPPORTED_RETRY);
            }
            // Not there yet: wait and try again, without drawing any
            // conclusion about what the other side knows how to do. The
            // wait can be long, but the poll cuts it short as soon as it
            // sees the server is back.
            Ok(Outcome::Unreachable) => {
                let d = retry.unreachable();
                nap(&handle, &uid, d);
            }
            Err(e) => {
                log::debug!("[watch] connection with {uid} ended: {e}");
                match retry.dropped(started.elapsed()) {
                    Next::Retry(d) => nap(&handle, &uid, d),
                    Next::StandDown => {
                        log::info!(
                            "[watch] {uid} keeps dropping the connection right away; leaving it to the periodic sync"
                        );
                        // The reference is useless after an hour without
                        // watching: on return you have to catch up anyway.
                        since = None;
                        nap(&handle, &uid, UNSUPPORTED_RETRY);
                    }
                }
            }
        }
    }
    log::info!("[watch] {uid} is no longer paired: stopping");
    if let Ok(mut threads) = handle.state::<AppState>().watchers.threads.lock() {
        threads.remove(&uid);
    }
}

/// One run against the server, waiting for it to finish.
///
/// The local network first, for the same reason as in `autosync`: pulling
/// over the LAN what the server would otherwise have to send is the
/// difference between one second and several minutes over the internet.
fn catch_up(handle: &AppHandle, uid: &str) {
    let lan = crate::autosync::lan_peers(handle);
    crate::pairing::wait_until_idle(handle, &lan, crate::autosync::LAN_FIRST_MAX_WAIT);
    crate::pairing::run_sync_blocking(handle.clone(), uid.to_string(), true);
}

/// Opens the connection, asks to be notified, and sits listening.
///
/// `since` comes in with the last known revision —with it the other side
/// answers right away whether we missed anything, instead of parking as if
/// nothing happened while we were disconnected— and **comes out updated**,
/// even if this ends in an error: that's what makes reconnection ask about
/// the latest and not about what it was when it connected.
fn wait_for_news(
    handle: &AppHandle,
    uid: &str,
    since: &mut Option<Mark>,
) -> anyhow::Result<Outcome> {
    // Not being able to connect isn't a waiting error: it's that there's
    // nobody to wait with yet. Distinguished here and not further up because
    // this is the only point that knows whether the session ever came to
    // exist.
    let mut sess = match crate::pairing::open_session_with(handle, uid, READ_TIMEOUT) {
        Ok(sess) => sess,
        Err(e) => {
            log::debug!("[watch] could not reach {uid}: {e}");
            return Ok(Outcome::Unreachable);
        }
    };
    sess.send(&Msg::Watch { since: *since })?;
    let _live = Live::start(handle, uid);
    loop {
        match sess.recv()? {
            // Still alive with no news. The reference moves anyway: if this
            // drops after hours of parking, reconnection asks about the
            // latest and not about yesterday's — which on a busy server gets
            // answered with a "maybe" and an extra sync.
            Msg::Ping { mark } => {
                *since = Some(mark);
                continue;
            }
            Msg::Changed { mark } => {
                *since = Some(mark);
                return Ok(Outcome::Changed);
            }
            Msg::Reject { reason } => {
                log::debug!("[watch] {uid} rejected the watch: {reason}");
                return Ok(Outcome::Unsupported);
            }
            other => return Err(anyhow::anyhow!("unexpected answer to Watch: {other:?}")),
        }
    }
}

/// While it exists, this device counts as connected and the reachability
/// poll skips it. It's dropped only when the connection ends, no matter how.
struct Live(AppHandle, String);

impl Live {
    fn start(handle: &AppHandle, uid: &str) -> Self {
        let state = handle.state::<AppState>();
        if let Ok(mut live) = state.watchers.live.lock() {
            live.insert(uid.to_string());
        }
        if state.peers.set_online(uid, true) {
            let _ = handle.emit("peers-changed", ());
        }
        Live(handle.clone(), uid.to_string())
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let state = self.0.state::<AppState>();
        // Its own variable and not an `if let` binding: there it would be a
        // temporary that outlives `state`, and it wouldn't compile.
        let live = state.watchers.live.lock();
        if let Ok(mut live) = live {
            live.remove(&self.1);
        }
        // Whether the device is down isn't decided here: the drop can be a
        // one-second blip. The poll picks it back up as soon as it leaves
        // the list and tells the truth within fifteen seconds.
    }
}

/// Sleeps, but with an ear open: anyone who finds out this device is back
/// within reach cuts the wait short.
fn nap(handle: &AppHandle, uid: &str, max: Duration) {
    handle.state::<AppState>().watchers.nap(uid, max);
}

/// This device's stored mark.
fn stored_mark(handle: &AppHandle, uid: &str) -> Option<Mark> {
    let state = handle.state::<AppState>();
    let conn = state.db.lock().ok()?;
    sway_core::pairing::watch_mark(&conn, uid)
}

/// Saves it. Only when it changed: the heartbeat arrives every 45 seconds
/// and almost always carries the same mark, and it's not worth taking the
/// database lock —the same one the screen needs— to write what was already
/// there.
fn remember_mark(handle: &AppHandle, uid: &str, mark: Option<Mark>) {
    let state = handle.state::<AppState>();
    let Ok(conn) = state.db.lock() else { return };
    if let Err(e) = sway_core::pairing::set_watch_mark(&conn, uid, mark) {
        log::debug!("[watch] could not store the mark of {uid}: {e}");
    }
}

fn still_paired(handle: &AppHandle, uid: &str) -> bool {
    let state = handle.state::<AppState>();
    let Ok(conn) = state.db.lock() else {
        return true; // couldn't check: not a reason to give up
    };
    conn.query_row("SELECT 1 FROM devices WHERE uid = ?1", [uid], |_| Ok(()))
        .is_ok()
}

/// Why there shouldn't be an open connection right now.
///
/// If automatic sync is off, or the network is metered and the user asked
/// not to spend it, being notified is useless: the notification would end
/// in a sync that isn't going to happen.
fn on_hold(handle: &AppHandle) -> Option<String> {
    let state = handle.state::<AppState>();
    let conditions = crate::power::current(&state);
    let conn = state.db.lock().ok()?;
    if !crate::autosync::enabled(&conn) {
        return Some("automatic sync is off".into());
    }
    crate::power::hold(&conditions, &crate::power::Limits::load(&conn), true)
        .map(|h| h.reason().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A powered-off server comes back. The wait grows to avoid hammering
    /// it, but it's never concluded that the server can't announce: counting
    /// it that way left devices not watching for an hour every time the
    /// server restarted — exactly when you're waiting to see if it works.
    #[test]
    fn a_powered_off_server_is_never_given_up_on() {
        let mut b = Backoff::new();
        for _ in 0..20 {
            let d = b.unreachable();
            assert!(d <= RETRY_MAX);
        }
        assert_eq!(b.wait, RETRY_MAX, "the wait has a ceiling");
        assert_eq!(b.quick_failures, 0, "it can't accumulate anything");
    }

    /// A connection that lived a while and dropped is the network here, not
    /// a downed server: it retries quickly. Without this, a phone that loses
    /// the connection every forty seconds ended up watching every five
    /// minutes, and the instant notice degraded on its own into a poll worse
    /// than the periodic tick.
    #[test]
    fn a_connection_that_was_alive_does_not_grow_the_wait() {
        let mut b = Backoff::new();
        // Before this, several drops left the wait high.
        b.unreachable();
        b.unreachable();
        assert!(b.wait > RETRY_MIN);

        assert_eq!(
            b.dropped(Duration::from_secs(45)),
            Next::Retry(RETRY_AFTER_DROP),
            "a connection that was running fine retries right away"
        );
        assert_eq!(b.wait, RETRY_MIN, "a healthy connection resets the wait");
    }

    /// A session that opens and drops on the spot, over and over, is a
    /// server that doesn't understand the request. That's not fixed by
    /// retrying.
    #[test]
    fn dropping_on_the_spot_several_times_means_stand_down() {
        let mut b = Backoff::new();
        let instant = Duration::from_millis(20);
        for _ in 0..TOO_QUICK_LIMIT - 1 {
            assert!(matches!(b.dropped(instant), Next::Retry(_)));
        }
        assert_eq!(b.dropped(instant), Next::StandDown);
    }

    /// And a single instant drop, among healthy connections, doesn't count
    /// toward that: it starts over from zero.
    #[test]
    fn a_lone_instant_drop_does_not_accumulate() {
        let mut b = Backoff::new();
        b.dropped(Duration::from_millis(20));
        b.dropped(Duration::from_secs(45));
        assert_eq!(b.quick_failures, 0);
        // And from there all of them are needed again.
        let instant = Duration::from_millis(20);
        for _ in 0..TOO_QUICK_LIMIT - 1 {
            assert!(matches!(b.dropped(instant), Next::Retry(_)));
        }
        assert_eq!(b.dropped(instant), Next::StandDown);
    }

    /// Without a bell, the wait lasts as long as it has to.
    #[test]
    fn without_a_bell_the_wait_runs_its_course() {
        let w = Watchers::default();
        let start = std::time::Instant::now();
        w.nap("srv", Duration::from_millis(120));
        assert!(start.elapsed() >= Duration::from_millis(100));
    }

    /// The poll sees the server is back and cuts the wait short. Without
    /// this, a server that turns on stays unwatched for up to five minutes,
    /// because the server can't announce and the watcher is asleep.
    #[test]
    fn the_bell_cuts_the_wait_short() {
        let w = std::sync::Arc::new(Watchers::default());
        let bg = std::sync::Arc::clone(&w);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            bg.wake("srv");
        });
        let start = std::time::Instant::now();
        w.nap("srv", Duration::from_secs(10));
        assert!(start.elapsed() < Duration::from_secs(5), "slept the full duration");
    }

    /// A bell that arrives while the watcher is busy isn't lost: the next
    /// wait ends immediately. Otherwise the news that the server is back
    /// gets dropped right when it arrives at the worst moment.
    #[test]
    fn a_bell_rung_before_the_wait_is_not_lost() {
        let w = Watchers::default();
        w.wake("srv");
        let start = std::time::Instant::now();
        w.nap("srv", Duration::from_secs(10));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    /// And it's for one only: one device's bell doesn't wake another's
    /// watcher.
    #[test]
    fn the_bell_is_for_one_only() {
        let w = Watchers::default();
        w.wake("other");
        let start = std::time::Instant::now();
        w.nap("srv", Duration::from_millis(120));
        assert!(start.elapsed() >= Duration::from_millis(100));
    }

    /// A notification erases everything before it: the connection worked
    /// end to end.
    #[test]
    fn a_notification_resets_everything_to_the_start() {
        let mut b = Backoff::new();
        b.unreachable();
        b.dropped(Duration::from_millis(20));
        b.news();
        assert_eq!(b.wait, RETRY_MIN);
        assert_eq!(b.quick_failures, 0);
    }
}
