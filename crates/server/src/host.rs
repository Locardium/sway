//! The sync engine running with nobody in front of it.
//!
//! The app implements `Host` over its `AppHandle` (backed by Tauri's state,
//! notifications as window events), and the integrity suite implements it
//! over a temporary directory. This is the third implementation, and the
//! flattest of the three: a database, a folder, and the rest goes to the log.

use anyhow::{anyhow, Result};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Condvar, Mutex};
use std::time::Duration;
use sway_core::engine::{Host, Progress, Seen};
use sway_core::wire::Mark;
use sway_core::rusqlite::Connection;

pub struct ServerHost {
    db: Mutex<Connection>,
    music_dir: PathBuf,
    /// What moved here and who moved it, so the others can be notified.
    changes: Mutex<Changes>,
    changed: Condvar,
}

/// How many changes are remembered along with their author.
///
/// Sixty-four is generous: seconds pass between a device starting to wait and
/// getting an answer, and it would take 64 changes from OTHER devices in that
/// window for one to be forgotten. And forgetting one costs nothing: it just
/// answers yes, meaning one sync too many, never one too few.
const REMEMBER: usize = 64;

#[derive(Default)]
struct Changes {
    /// Identifies THIS run of the server. Randomized on startup.
    ///
    /// Without this, today's revision 57 and the revision 57 after a restart
    /// are the same number, and a device with a saved mark would conclude
    /// it's up to date right when everything in between was lost.
    epoch: u64,
    /// How many times the library moved since the process started. It isn't
    /// persistent and doesn't need to be: a restart drops the connections
    /// that were waiting, so nobody is left holding a stale number.
    rev: u64,
    /// The most recent ones, with the uid of whoever made them.
    recent: VecDeque<(u64, String)>,
}

impl Changes {
    fn mark(&self) -> Mark {
        Mark { epoch: self.epoch, rev: self.rev }
    }

    /// Did something happen after `since` that `ignoring` doesn't already know about?
    ///
    /// The author matters: without this, the device that pushes a change
    /// wakes itself up and goes out to sync again the very thing it just
    /// sent — a whole manifest over the internet, on every local change.
    fn news_for(&self, since: u64, ignoring: &str) -> bool {
        if self.rev <= since {
            return false;
        }
        // Part of what happened since then was forgotten: there's no way to
        // know whose it was, and staying quiet would be worse than one sync
        // too many.
        if self.recent.front().map(|(r, _)| *r > since + 1).unwrap_or(true) {
            return true;
        }
        self.recent.iter().any(|(r, who)| *r > since && who != ignoring)
    }
}

impl ServerHost {
    pub fn new(conn: Connection, music_dir: PathBuf) -> Self {
        Self {
            db: Mutex::new(conn),
            music_dir,
            changes: Mutex::new(Changes {
                // Random and not the time: two startups within the same
                // millisecond would produce the same value, and "unlikely"
                // isn't "never happens".
                epoch: uuid::Uuid::new_v4().as_u128() as u64,
                ..Changes::default()
            }),
            changed: Condvar::new(),
        }
    }
}

impl Host for ServerHost {
    fn with_db<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.db.lock().map_err(|_| anyhow!("poisoned db lock"))?;
        f(&conn)
    }

    // `with_db_read` stays as the default (the same connection). The app has
    // a second read-only connection because its screen can't be left waiting
    // for sync to release the lock; there's no screen here.

    fn music_dir(&self) -> PathBuf {
        self.music_dir.clone()
    }

    // `expect_path` does nothing: there's no folder watcher, so nobody is
    // going to try to auto-import what sync leaves behind.

    fn progress(&self, p: &Progress) {
        // Only the ends of each file. A server that runs for weeks can't
        // write a line per chunk.
        if p.done == 0 {
            log::info!(
                "[sync] {} {}/{}: {}",
                if p.sending { "sending" } else { "receiving" },
                p.index + 1,
                p.total_files,
                p.filename
            );
        } else if p.done >= p.total && p.total > 0 {
            log::info!("[sync] done {} ({} bytes)", p.filename, p.total);
        }
    }

    // `library_changed` still does nothing: there's no UI to reload.
    // Notifying the others is a separate thing, and goes through
    // `note_change_by`, which does know who caused the change.

    fn note_change_by(&self, peer_uid: &str) {
        let Ok(mut c) = self.changes.lock() else { return };
        c.rev += 1;
        let rev = c.rev;
        c.recent.push_back((rev, peer_uid.to_string()));
        while c.recent.len() > REMEMBER {
            c.recent.pop_front();
        }
        // To everyone waiting: there can be several devices at once, and each
        // one decides on its own whether the change matters to it.
        self.changed.notify_all();
    }

    fn revision(&self) -> Option<Mark> {
        self.changes.lock().ok().map(|c| c.mark())
    }

    fn wait_revision(&self, since: Mark, ignoring: &str, max: Duration) -> Seen {
        let Ok(changes) = self.changes.lock() else {
            return Seen { news: false, mark: since };
        };
        match self
            .changed
            .wait_timeout_while(changes, max, |c| !c.news_for(since.rev, ignoring))
        {
            // Both answers come from the same guard: if the revision were
            // read after releasing it, a change landing in between would
            // sneak into the mark sent as "you're up to date".
            Ok((c, _)) => Seen {
                news: c.news_for(since.rev, ignoring),
                mark: c.mark(),
            },
            Err(_) => Seen { news: false, mark: since },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PC: &str = "uid-pc";
    const PHONE: &str = "uid-phone";

    fn host() -> ServerHost {
        ServerHost::new(Connection::open_in_memory().unwrap(), PathBuf::from("."))
    }

    /// With no changes, the wait times out on its own. This later translates
    /// into a heartbeat: the connection is still alive and nothing was
    /// announced.
    #[test]
    fn with_no_changes_the_wait_times_out() {
        let h = host();
        let rev = h.revision().unwrap();
        assert!(!h.wait_revision(rev, PC, Duration::from_millis(80)).news);
    }

    /// Another device's change cuts the wait short right away, without using up the deadline.
    #[test]
    fn another_devices_change_cuts_the_wait_short() {
        let h = std::sync::Arc::new(host());
        let rev = h.revision().unwrap();
        let bg = std::sync::Arc::clone(&h);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            bg.note_change_by(PHONE);
        });
        let start = std::time::Instant::now();
        assert!(h.wait_revision(rev, PC, Duration::from_secs(10)).news);
        assert!(start.elapsed() < Duration::from_secs(5), "waited out the full deadline");
    }

    /// What you pushed yourself isn't news to yourself. Without this, every
    /// local change ends up syncing back against the archive with nothing to
    /// bring.
    #[test]
    fn your_own_push_does_not_wake_you_up() {
        let h = host();
        let rev = h.revision().unwrap();
        h.note_change_by(PC);
        assert!(!h.wait_revision(rev, PC, Duration::from_millis(80)).news);
        // The other one does care, though.
        assert!(h.wait_revision(rev, PHONE, Duration::from_millis(80)).news);
    }

    /// A change that happened BEFORE the wait started also counts: the
    /// reference is set by whoever is waiting. Without this, a change landing
    /// between two heartbeats wouldn't wake anyone.
    #[test]
    fn a_change_before_the_wait_started_still_counts() {
        let h = host();
        let rev = h.revision().unwrap();
        h.note_change_by(PHONE);
        assert!(h.wait_revision(rev, PC, Duration::from_millis(80)).news);
    }

    /// What gets reported as "you're up to date" and the revision sent out
    /// come from the same look. If they were read separately, the heartbeat
    /// could say "up to date at revision N" with news already inside N, and
    /// whoever's waiting would move its reference past something that never
    /// actually arrived.
    #[test]
    fn the_reported_revision_is_the_one_that_was_looked_at() {
        let h = host();
        let rev = h.revision().unwrap();
        h.note_change_by(PC);
        h.note_change_by(PC);
        let seen = h.wait_revision(rev, PC, Duration::from_millis(80));
        assert!(!seen.news, "they were its own");
        assert_eq!(
            seen.mark.rev,
            rev.rev + 2,
            "the reported revision has to be the current one"
        );
        assert_eq!(seen.mark.epoch, rev.epoch, "the run doesn't change on its own");
    }

    /// With more changes than are remembered, it's no longer known whose each
    /// one was, so it announces anyway.
    #[test]
    fn when_the_old_ones_are_forgotten_it_still_announces() {
        let h = host();
        let rev = h.revision().unwrap();
        for _ in 0..REMEMBER + 1 {
            h.note_change_by(PC);
        }
        assert!(h.wait_revision(rev, PC, Duration::from_millis(80)).news);
    }
}
