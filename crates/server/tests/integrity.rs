//! Archive integrity: a real server and real devices, over loopback.
//!
//! Phase 5.8 left two engines syncing within a single process so cases that
//! can't be tested by hand could be tested. This adds the third participant,
//! which is the one that brings the new cases: a device that lost everything
//! and recovers it, and —the one that can never fail— an empty device that
//! does **not** convince the archive that there was nothing.
//!
//! Nothing is simulated: the server is the real binary, with its socket, its
//! database and its thread; the devices speak the real protocol and move
//! real bytes.

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use sway_core::engine::{self, Host, Progress};
use sway_core::rusqlite::Connection;
use sway_core::wire::{Mark, Msg, Session};
use sway_core::{db, pairing};
use sway_server::host::ServerHost;
use sway_server::serve::{self, Server};

const TOKEN: &str = "suite-token";
/// No test moves more than a few kilobytes: if something is left waiting it's
/// a bug, and an error is worth more than a suite that hangs.
const IO_TIMEOUT: Duration = Duration::from_secs(20);

fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sway-int-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn mem_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    db::init_schema(&conn).unwrap();
    db::this_device_uid(&conn).unwrap();
    conn
}

// ---------------------------------------------------------------------------
// A device
// ---------------------------------------------------------------------------

/// The bare minimum the engine needs from a real device: a database and a
/// folder where the files live.
struct Device {
    dir: PathBuf,
    db: Mutex<Connection>,
}

impl Host for Device {
    fn with_db<T>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let conn = self.db.lock().map_err(|_| anyhow::anyhow!("db lock"))?;
        f(&conn)
    }
    fn music_dir(&self) -> PathBuf {
        self.dir.clone()
    }
    fn progress(&self, _p: &Progress) {}
}

impl Device {
    fn new(tag: &str) -> Self {
        Device {
            dir: tmpdir(tag),
            db: Mutex::new(mem_db()),
        }
    }

    fn uid(&self) -> String {
        self.with_db(|c| Ok(db::this_device_uid(c)?)).unwrap()
    }

    /// A track with a real file in the managed folder.
    fn add_track(&self, filename: &str, bytes: &[u8], title: &str) -> String {
        let path = self.dir.join(filename);
        std::fs::write(&path, bytes).unwrap();
        let hash = sway_core::hashing::hash_file(&path).unwrap();
        let uid = db::new_uid();
        let conn = self.db.lock().unwrap();
        sway_core::transfer::insert_received(
            &conn,
            &path,
            &uid,
            &hash,
            bytes.len() as u64,
            title,
            "Artist",
            "",
            "",
            0,
            None,
            db::now_ms(),
        )
        .unwrap();
        uid
    }

    fn delete_track(&self, uid: &str) {
        let conn = self.db.lock().unwrap();
        let path: String = conn
            .query_row("SELECT path FROM tracks WHERE uid = ?1", [uid], |r| r.get(0))
            .unwrap();
        conn.execute("DELETE FROM tracks WHERE uid = ?1", [uid]).unwrap();
        db::record_tombstone(&conn, "track", uid).unwrap();
        std::fs::remove_file(path).ok();
    }

    fn track_uids(&self) -> Vec<String> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn.prepare("SELECT uid FROM tracks ORDER BY uid").unwrap();
        let v: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        v
    }

    fn audio_files(&self) -> Vec<PathBuf> {
        audio_files_in(&self.dir)
    }

    /// Pairs with the server and is ready to sync.
    fn pair_with(&self, addr: &str, server_uid: &str) {
        let mut sess = self.connect(addr);
        let (uid, name) = self.with_db(|c| Ok(pairing::me(c)?)).unwrap();
        sess.send(&Msg::PairRequest {
            uid,
            name,
            platform: "test".into(),
            token: Some(TOKEN.into()),
        })
        .unwrap();
        match sess.recv().unwrap() {
            Msg::PairResponse { accepted } => assert!(accepted, "the server rejected the token"),
            other => panic!("expected PairResponse, got {other:?}"),
        }
        sess.send(&Msg::PairAck { accepted: true }).unwrap();
        let key = match sess.recv().unwrap() {
            Msg::Hello { .. } => sess.peer_pubkey.clone(),
            other => panic!("expected Hello, got {other:?}"),
        };
        sess.send(&self.hello()).unwrap();
        drop(sess);

        // The pairing is also saved on the device's side: without the row,
        // syncing afterward would be talking to a stranger.
        self.with_db(|c| pairing::store_device(c, server_uid, "Server", "server", &key))
            .unwrap();
    }

    fn hello(&self) -> Msg {
        let (uid, name) = self.with_db(|c| Ok(pairing::me(c)?)).unwrap();
        let (tracks, playlists) = self.with_db(|c| Ok(pairing::library_counts(c))).unwrap();
        Msg::Hello {
            uid,
            name,
            platform: "test".into(),
            tracks,
            playlists,
            clock_ms: db::now_ms(),
        }
    }

    fn connect(&self, addr: &str) -> Session {
        self.connect_with(addr, IO_TIMEOUT)
    }

    fn connect_with(&self, addr: &str, read_timeout: Duration) -> Session {
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(read_timeout)).unwrap();
        stream.set_write_timeout(Some(IO_TIMEOUT)).unwrap();
        let private = self.with_db(|c| Ok(pairing::keypair(c)?.0)).unwrap();
        Session::connect(stream, &private).unwrap()
    }

    /// Waits for the server to announce news, the way the app's watcher does.
    /// Returns the revision it was told about, or `None` if patience ran out
    /// with nobody saying anything.
    ///
    /// `since` is the last known revision, just like in the watcher: with it
    /// the server answers right away with what happened while we were away.
    fn wait_for_news(
        &self,
        addr: &str,
        patience: Duration,
        since: Option<Mark>,
    ) -> Option<Mark> {
        let mut sess = self.connect_with(addr, patience);
        sess.send(&self.hello()).unwrap();
        match sess.recv().unwrap() {
            Msg::Hello { .. } => {}
            other => panic!("expected Hello, got {other:?}"),
        }
        sess.send(&Msg::Watch { since }).unwrap();
        loop {
            match sess.recv() {
                Ok(Msg::Ping { .. }) => continue,
                Ok(Msg::Changed { mark }) => return Some(mark),
                // The read deadline ran out: nobody announced anything.
                Err(_) => return None,
                Ok(other) => panic!("unexpected response to Watch: {other:?}"),
            }
        }
    }

    /// A full run against the server, the same one the app triggers.
    fn sync_with(&self, addr: &str, server_uid: &str) -> engine::SyncResult {
        let mut sess = self.connect(addr);
        sess.send(&self.hello()).unwrap();
        match sess.recv().unwrap() {
            Msg::Hello { .. } => {}
            other => panic!("expected Hello, got {other:?}"),
        }
        let out = engine::sync(self, &mut sess, server_uid).unwrap();
        let _ = sess.send(&Msg::Bye);
        out
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

// ---------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------

struct Archive {
    server: Arc<Server>,
    addr: String,
    dir: PathBuf,
}

impl Archive {
    fn start(tag: &str) -> Self {
        let dir = tmpdir(tag);
        let conn = mem_db();
        db::set_device_name(&conn, "Test Server").unwrap();
        let host = Arc::new(ServerHost::new(conn, dir.clone()));
        // Same as the binary: the archive declares itself as wanting
        // everything, in both directions.
        host.with_db(|c| {
            let me = db::this_device_uid(c)?;
            sway_core::scope::set_mode(c, &me, sway_core::scope::Mode::All)?;
            sway_core::scope::set_direction(c, &me, "both")?;
            Ok(())
        })
        .unwrap();

        let server = Arc::new(Server {
            host,
            token: TOKEN.into(),
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let bg = Arc::clone(&server);
        std::thread::spawn(move || {
            let _ = serve::run(bg, listener);
        });
        Archive { server, addr, dir }
    }

    fn uid(&self) -> String {
        self.server
            .host
            .with_db(|c| Ok(db::this_device_uid(c)?))
            .unwrap()
    }

    /// How far the archive has gotten right now.
    fn mark(&self) -> Mark {
        self.server.host.revision().unwrap()
    }

    fn track_titles(&self) -> Vec<String> {
        self.server
            .host
            .with_db(|c| {
                let mut stmt = c.prepare("SELECT title FROM tracks ORDER BY title")?;
                let v: Vec<String> = stmt
                    .query_map([], |r| r.get(0))?
                    .map(|r| r.unwrap())
                    .collect();
                Ok(v)
            })
            .unwrap()
    }

    fn audio_files(&self) -> Vec<PathBuf> {
        audio_files_in(&self.dir)
    }

    /// What's left in the server's trash.
    fn trashed(&self) -> Vec<Vec<u8>> {
        std::fs::read_dir(sway_core::trash::trash_dir(&self.dir))
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().is_file())
                    .map(|e| std::fs::read(e.path()).unwrap())
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl Drop for Archive {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn audio_files_in(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().is_file())
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

fn audio(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| ((i + seed as usize) % 251) as u8).collect()
}

// ---------------------------------------------------------------------------

/// The base case: what a device has ends up in the archive, bytes and all,
/// not just the row.
#[test]
fn what_a_device_has_ends_up_in_the_archive() {
    let srv = Archive::start("sube");
    let pc = Device::new("sube-pc");
    let srv_uid = srv.uid();
    pc.pair_with(&srv.addr, &srv_uid);

    pc.add_track("uno.flac", &audio(4096, 1), "One");
    pc.add_track("dos.flac", &audio(4096, 2), "Two");
    let r = pc.sync_with(&srv.addr, &srv_uid);

    assert_eq!(r.sent, 2, "both files had to travel");
    assert_eq!(srv.track_titles(), vec!["One", "Two"]);
    assert_eq!(srv.audio_files().len(), 2, "and end up on disk, not just in the database");
}

/// Syncing again without touching anything moves zero bytes. Converging once
/// is easy; staying still afterward is what tends to break.
#[test]
fn the_second_run_moves_nothing() {
    let srv = Archive::start("quieto");
    let pc = Device::new("quieto-pc");
    let srv_uid = srv.uid();
    pc.pair_with(&srv.addr, &srv_uid);
    pc.add_track("uno.flac", &audio(2048, 3), "One");

    pc.sync_with(&srv.addr, &srv_uid);
    let second = pc.sync_with(&srv.addr, &srv_uid);

    assert_eq!((second.sent, second.received), (0, 0));
    assert_eq!(second.organized, 0, "no organization either");
}

/// You reinstalled the app and nothing's left. You pair again with the token
/// and sync: everything comes back, files included.
#[test]
fn a_device_that_lost_everything_recovers_it_from_the_archive() {
    let srv = Archive::start("restore");
    let srv_uid = srv.uid();

    let old = Device::new("restore-viejo");
    old.pair_with(&srv.addr, &srv_uid);
    old.add_track("uno.flac", &audio(4096, 11), "One");
    old.add_track("dos.flac", &audio(4096, 12), "Two");
    old.sync_with(&srv.addr, &srv_uid);
    drop(old);

    // The reinstall is a new device: new identity, empty database, empty
    // folder. The only thing it brings is the token.
    let new = Device::new("restore-nuevo");
    new.pair_with(&srv.addr, &srv_uid);
    assert!(new.track_uids().is_empty());

    let r = new.sync_with(&srv.addr, &srv_uid);

    assert_eq!(r.received, 2, "both had to come down");
    assert_eq!(new.track_uids().len(), 2);
    assert_eq!(new.audio_files().len(), 2, "the files, not just the rows");
}

/// **The one that can never fail.**
///
/// A device with an empty database has no tombstones: it's not saying
/// something was deleted, it's saying it doesn't know anything. If the
/// archive interpreted that silence as a deletion, a factory-reset phone
/// would take down the only remaining copy of everything with it.
#[test]
fn an_empty_device_deletes_nothing_from_the_archive() {
    let srv = Archive::start("vacio");
    let srv_uid = srv.uid();

    let pc = Device::new("vacio-pc");
    pc.pair_with(&srv.addr, &srv_uid);
    pc.add_track("uno.flac", &audio(4096, 21), "One");
    pc.add_track("dos.flac", &audio(4096, 22), "Two");
    pc.sync_with(&srv.addr, &srv_uid);
    assert_eq!(srv.track_titles().len(), 2);

    let reset = Device::new("vacio-reset");
    reset.pair_with(&srv.addr, &srv_uid);
    reset.sync_with(&srv.addr, &srv_uid);

    assert_eq!(srv.track_titles(), vec!["One", "Two"], "the archive got emptied");
    assert_eq!(srv.audio_files().len(), 2, "and lost the files");

    // And the one that stayed alive loses nothing either the next time it syncs.
    pc.sync_with(&srv.addr, &srv_uid);
    assert_eq!(pc.track_uids().len(), 2);
    assert_eq!(pc.audio_files().len(), 2);
}

/// A deletion does travel, and on the server the file isn't destroyed: it
/// stays in its trash, which is the only thing that makes an accidental
/// deletion recoverable.
#[test]
fn a_deletion_travels_but_the_file_stays_in_the_servers_trash() {
    let srv = Archive::start("borrado");
    let srv_uid = srv.uid();
    let pc = Device::new("borrado-pc");
    pc.pair_with(&srv.addr, &srv_uid);

    let bytes = audio(4096, 31);
    let uid = pc.add_track("uno.flac", &bytes, "One");
    pc.add_track("dos.flac", &audio(4096, 32), "Two");
    pc.sync_with(&srv.addr, &srv_uid);
    assert_eq!(srv.track_titles().len(), 2);

    pc.delete_track(&uid);
    pc.sync_with(&srv.addr, &srv_uid);

    assert_eq!(srv.track_titles(), vec!["Two"], "the deletion had to arrive");
    assert_eq!(srv.audio_files().len(), 1);
    assert!(
        srv.trashed().contains(&bytes),
        "but the bytes are still recoverable in the server's trash"
    );

    // And it doesn't come back on the next pass.
    let another = pc.sync_with(&srv.addr, &srv_uid);
    assert_eq!((another.sent, another.received), (0, 0));
    assert_eq!(pc.track_uids().len(), 1);
}

/// Two devices that never see each other, each on its own network: the
/// archive is what connects them. This is the case that motivated the whole
/// phase.
#[test]
fn two_devices_that_cannot_see_each_other_sync_through_the_archive() {
    let srv = Archive::start("puente");
    let srv_uid = srv.uid();

    let phone = Device::new("puente-celu");
    phone.pair_with(&srv.addr, &srv_uid);
    phone.add_track("de-afuera.flac", &audio(4096, 41), "From outside");
    phone.sync_with(&srv.addr, &srv_uid);

    // The other one was off while all this was happening.
    let pc = Device::new("puente-pc");
    pc.pair_with(&srv.addr, &srv_uid);
    let r = pc.sync_with(&srv.addr, &srv_uid);

    assert_eq!(r.received, 1);
    assert_eq!(pc.audio_files().len(), 1, "it arrived without the phone being present");
}

/// Phase 6's gap: a change made away from home reaches the server right
/// away, but the other device has no way of finding out because the server
/// doesn't call anyone. With `Watch` it does: whoever's waiting finds out as
/// soon as the archive moves, without polling periodically.
#[test]
fn a_change_from_another_device_wakes_up_the_one_waiting() {
    let srv = Archive::start("aviso");
    let srv_uid = srv.uid();

    let pc = Device::new("aviso-pc");
    pc.pair_with(&srv.addr, &srv_uid);
    // Up to date before starting to wait: that's what the watcher does, and
    // without it the test wouldn't be able to tell a notification apart from
    // an initial catch-up.
    pc.sync_with(&srv.addr, &srv_uid);

    let phone = Device::new("aviso-celu");
    phone.pair_with(&srv.addr, &srv_uid);
    phone.add_track("recien-importado.flac", &audio(4096, 77), "Just imported");

    std::thread::scope(|s| {
        let waiting = s.spawn(|| pc.wait_for_news(&srv.addr, IO_TIMEOUT, None));
        // Make sure the wait is actually parked before moving anything: if
        // the change happened first, the notification would still arrive but
        // the test wouldn't be testing what it claims to.
        std::thread::sleep(Duration::from_millis(300));
        phone.sync_with(&srv.addr, &srv_uid);
        assert!(
            waiting.join().unwrap().is_some(),
            "the server did not announce a change it had just applied"
        );
    });

    // And what follows the notification is a normal sync, which brings the track.
    let r = pc.sync_with(&srv.addr, &srv_uid);
    assert_eq!(r.received, 1);
}

/// With no news, nothing gets announced: one notification too many sends
/// every device off to sync for no reason, and against the archive that's a
/// whole manifest over the internet.
#[test]
fn with_no_changes_nobody_gets_notified() {
    let srv = Archive::start("silencio");
    let srv_uid = srv.uid();

    let pc = Device::new("silencio-pc");
    pc.pair_with(&srv.addr, &srv_uid);
    pc.add_track("propio.flac", &audio(2048, 12), "Own track");
    // This device's own sync moves the archive's library, but it's not news
    // TO IT: by the time it starts waiting, its own change is already on the
    // old side of the comparison.
    pc.sync_with(&srv.addr, &srv_uid);

    // This test cuts the wait short, not the server: on the other end it
    // stays waiting until something happens, which is exactly what's being
    // tested.
    const PATIENCE: Duration = Duration::from_millis(1500);
    let start = std::time::Instant::now();
    assert!(
        pc.wait_for_news(&srv.addr, PATIENCE, None).is_none(),
        "announced a change that does not exist"
    );
    assert!(
        start.elapsed() >= PATIENCE,
        "cut short instead of actually waiting"
    );
}

/// What a device pushes isn't news TO THAT DEVICE, even if it was waiting
/// when it pushed it. Without this distinction, every time you import
/// something the notification bounces back to you and you go sync against
/// the archive the very thing you just sent: a whole manifest over the
/// internet, for nothing, on every local change.
#[test]
fn what_a_device_pushes_itself_does_not_wake_it_up() {
    let srv = Archive::start("rebote");
    let srv_uid = srv.uid();

    let pc = Device::new("rebote-pc");
    pc.pair_with(&srv.addr, &srv_uid);
    pc.sync_with(&srv.addr, &srv_uid);

    const PATIENCE: Duration = Duration::from_millis(1500);
    std::thread::scope(|s| {
        let waiting = s.spawn(|| pc.wait_for_news(&srv.addr, PATIENCE, None));
        std::thread::sleep(Duration::from_millis(300));
        // With the wait already parked, this same device pushes something —
        // which is what automatic sync does when you import music.
        pc.add_track("importado-recien.flac", &audio(4096, 31), "Just imported");
        pc.sync_with(&srv.addr, &srv_uid);
        assert!(
            waiting.join().unwrap().is_none(),
            "it was notified of its own change"
        );
    });
}

/// Reconnecting has to be cheap. On a phone the connection doesn't last a
/// minute —it switches from wifi to mobile data, the carrier's NAT cuts it—,
/// so it reconnects constantly; if every time it had to drag along a full
/// catch-up just in case, this would end up costing more than the periodic
/// polling it was meant to improve on.
///
/// With the known revision it doesn't have to: the server knows what
/// happened while we were away and answers on the spot, without parking and
/// without anyone having to sync just to find out.
#[test]
fn reconnecting_tells_you_what_you_missed_without_syncing() {
    let srv = Archive::start("reconecta");
    let srv_uid = srv.uid();

    let pc = Device::new("reconecta-pc");
    pc.pair_with(&srv.addr, &srv_uid);
    pc.sync_with(&srv.addr, &srv_uid);

    // What the PC knows about the archive before anything happens.
    let known = srv.mark();
    let up_to_date = pc
        .wait_for_news(&srv.addr, Duration::from_millis(300), None)
        .is_none();
    assert!(up_to_date, "there was no news yet");

    // With the PC disconnected, the phone pushes something.
    let phone = Device::new("reconecta-celu");
    phone.pair_with(&srv.addr, &srv_uid);
    phone.add_track("mientras-no-estabas.flac", &audio(4096, 55), "While you were away");
    phone.sync_with(&srv.addr, &srv_uid);

    // The PC comes back asking about the mark it had. It has to answer on
    // the spot, without waiting out the full deadline.
    let start = std::time::Instant::now();
    let rev = pc.wait_for_news(&srv.addr, IO_TIMEOUT, Some(known));
    assert!(rev.is_some(), "did not report what was missed");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "parked instead of answering what had already happened"
    );

    // And with the up-to-date revision it parks again, without repeating the notification.
    let again = pc.wait_for_news(&srv.addr, Duration::from_millis(500), rev);
    assert!(again.is_none(), "repeated a notification already given");
}

/// If the server restarted, its count starts over from zero and the device's
/// reference points into the future. There's no way to know what happened
/// before, so it announces and lets it come take a look — staying quiet
/// would leave the device waiting forever for a revision that's never coming.
#[test]
fn a_revision_from_the_future_sends_it_off_to_sync() {
    let srv = Archive::start("reinicio");
    let srv_uid = srv.uid();

    let pc = Device::new("reinicio-pc");
    pc.pair_with(&srv.addr, &srv_uid);
    pc.sync_with(&srv.addr, &srv_uid);

    let current = srv.mark();
    let from_the_future = Mark { epoch: current.epoch, rev: current.rev + 9999 };
    let start = std::time::Instant::now();
    let rev = pc.wait_for_news(&srv.addr, IO_TIMEOUT, Some(from_the_future));
    assert_eq!(rev, Some(current), "had to return the server's real mark");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "parked instead of sending it off to sync"
    );
}

/// The uncompressed path has to keep working: it's the one used by a device
/// that hasn't updated yet, and breaking it would cut sync between a new end
/// and an old one — considerably worse than it just being slow.
///
/// The rest of this suite's tests already exercise the compressed path,
/// since the engine asks for `gzip: true`. This is the other half.
#[test]
fn a_device_that_cannot_gzip_gets_the_usual_inventory() {
    let srv = Archive::start("sin-gzip");
    let srv_uid = srv.uid();

    let pc = Device::new("sin-gzip-pc");
    pc.pair_with(&srv.addr, &srv_uid);
    pc.add_track("un-tema.flac", &audio(2048, 9), "A track");
    pc.sync_with(&srv.addr, &srv_uid);

    let mut sess = pc.connect(&srv.addr);
    sess.send(&pc.hello()).unwrap();
    match sess.recv().unwrap() {
        Msg::Hello { .. } => {}
        other => panic!("expected Hello, got {other:?}"),
    }
    // Exactly what an older version sends.
    sess.send(&Msg::ManifestReq { gzip: false }).unwrap();
    match sess.recv().unwrap() {
        Msg::ManifestData { manifest } => {
            assert_eq!(manifest.tracks.len(), 1, "the inventory has to come in full");
        }
        Msg::ManifestGz { .. } => panic!("sent compressed data to someone who did not ask for it"),
        other => panic!("unexpected response: {other:?}"),
    }
}

/// The gap that saving the mark to disk uncovers: the server's revision
/// count lives in memory and starts over from zero on every restart, so
/// today's revision 57 and tomorrow's are the same number. A device that
/// comes back with a saved mark would conclude it's up to date right when
/// everything in between was lost.
///
/// That's why the mark also carries which run of the server it was: another
/// run is always "I don't know you, come compare", even if the numbers match.
#[test]
fn a_mark_from_another_run_sends_it_off_to_compare() {
    let srv = Archive::start("corrida");
    let srv_uid = srv.uid();

    let pc = Device::new("corrida-pc");
    pc.pair_with(&srv.addr, &srv_uid);
    pc.sync_with(&srv.addr, &srv_uid);

    let current = srv.mark();
    // Same exact revision, another run: this is exactly the case that,
    // without `epoch`, would have read as "nothing happened".
    let earlier = Mark { epoch: current.epoch.wrapping_add(1), rev: current.rev };

    let start = std::time::Instant::now();
    let answered = pc.wait_for_news(&srv.addr, IO_TIMEOUT, Some(earlier));
    assert_eq!(
        answered,
        Some(current),
        "stayed quiet with the same revision from another run"
    );
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "parked instead of sending it off to compare"
    );
}
