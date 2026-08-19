//! Audio file transfer (Phase 5.4).
//!
//! The hard requirement of all of Phase 5 is that music must never be lost.
//! That's not achieved by being careful, it's achieved by a path that has no
//! way to lose anything:
//!
//! ```text
//! 1. request the file from byte N (N = whatever is already in the .part)
//! 2. write to  <music>/.sway-incoming/<hash>.part
//! 3. network cut / app killed -> the .part survives, resumes from there
//! 4. complete -> blake3 of the WHOLE .part
//!      different  -> the .part is deleted, the library was never touched
//!      same       -> rename() to the final destination (atomic, same filesystem)
//! 5. only then, the row in the DB
//! ```
//!
//! Invariants:
//! - The final destination is **never** written live: everything goes through the .part.
//! - If the destination exists with different content, the name is disambiguated. An
//!   existing audio file is never overwritten.
//! - The whole file is verified, not just what just arrived: a .part with a
//!   corrupt prefix from a previous run also has to fail.

use crate::wire::{Msg, Session};
use anyhow::{anyhow, Result};
use rusqlite::OptionalExtension;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Size of each raw payload. The channel internally splits it into 64 KiB
/// frames (Noise's cap); this is just how much is read from disk per round.
const CHUNK: usize = 1024 * 1024;

/// Half-finished downloads. Deliberately placed inside the managed folder:
/// `rename()` is only atomic within the same filesystem, and on Android the
/// app folder and the music folder can be on different volumes.
pub fn incoming_dir(music_dir: &Path) -> PathBuf {
    music_dir.join(".sway-incoming")
}

fn part_path(music_dir: &Path, hash: &str) -> PathBuf {
    incoming_dir(music_dir).join(format!("{hash}.part"))
}

/// How much of this file has already been downloaded.
fn resume_offset(music_dir: &Path, hash: &str) -> u64 {
    std::fs::metadata(part_path(music_dir, hash))
        .map(|m| m.len())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Sending side
// ---------------------------------------------------------------------------

/// Sends `path` starting at `offset`. The receiver already knows the expected hash.
pub fn send_file(sess: &mut Session, path: &Path, offset: u64, hash: &str) -> Result<()> {
    let mut f = std::fs::File::open(path)?;
    let total = f.metadata()?.len();
    if offset > total {
        return Err(anyhow!("offset {offset} is past the end of the file ({total})"));
    }
    f.seek(SeekFrom::Start(offset))?;
    sess.send(&Msg::BlobStart {
        size: total - offset,
    })?;

    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        sess.send_bytes(&buf[..n])?;
    }
    sess.send(&Msg::BlobEnd {
        hash: hash.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Receiving side
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Received {
    pub path: PathBuf,
    pub bytes: u64,
}

/// Receives a file that was already requested (or pushed) and leaves it in the
/// managed folder. Returns the final path.
///
/// `progress(received, total)` is called often: with 40 MB files over a home
/// network, without this the UI looks frozen.
/// `mark_expected` is called with the destination **before** the rename. The
/// managed folder's watcher uses that to avoid auto-importing what sync just
/// brought in: if it imported it, it would create a row with a new uid and
/// overwrite the metadata synced from the file's tags.
///
/// `resuming` says whether what's coming continues a `.part` that already
/// existed. **It's only true when this side requested the file with an
/// offset** (`pull_file`). In a push the other side sends from scratch without
/// knowing what we have, so an old `.part` with the same hash has to be
/// discarded: stacking the whole file on top of it gives a longer file, a
/// different hash, and an entire transfer wasted that repeats on every sync.
pub fn receive_file(
    sess: &mut Session,
    music_dir: &Path,
    hash: &str,
    filename: &str,
    resuming: bool,
    progress: &mut dyn FnMut(u64, u64),
    mark_expected: &mut dyn FnMut(&Path),
) -> Result<Received> {
    let incoming = incoming_dir(music_dir);
    std::fs::create_dir_all(&incoming)?;
    let part = part_path(music_dir, hash);
    if !resuming && part.exists() {
        log::debug!("[sync] discarding a stale partial of {hash}");
        let _ = std::fs::remove_file(&part);
    }

    let expected = match sess.recv()? {
        Msg::BlobStart { size } => size,
        Msg::BlobError { reason } => return Err(anyhow!(reason)),
        other => return Err(anyhow!("expected BlobStart, got {other:?}")),
    };

    // Append: what was already there is kept; that's why the request carried an offset.
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&part)?;
    let already = f.metadata()?.len();
    let total = already + expected;
    let mut got = already;
    progress(got, total);

    while got < total {
        let chunk = sess.recv_bytes()?;
        if chunk.is_empty() {
            return Err(anyhow!("the sender stopped at {got} of {total} bytes"));
        }
        f.write_all(&chunk)?;
        got += chunk.len() as u64;
        progress(got, total);
    }
    // To disk before considering the download good: if power is cut here, what's
    // left in the .part has to be what was actually written.
    f.flush()?;
    f.sync_all()?;
    drop(f);

    match sess.recv()? {
        Msg::BlobEnd { hash: sent } if sent == hash => {}
        Msg::BlobEnd { hash: sent } => {
            let _ = std::fs::remove_file(&part);
            return Err(anyhow!("the sender reports having sent {sent}, {hash} was requested"));
        }
        other => return Err(anyhow!("expected BlobEnd, got {other:?}")),
    }

    // The WHOLE file is verified, not just what just arrived: a .part with a
    // corrupt prefix from a previous run also has to fail.
    let actual = crate::hashing::hash_file(&part)?;
    if actual != hash {
        // The library was never touched: the only thing lost is the download.
        let _ = std::fs::remove_file(&part);
        return Err(anyhow!("hash mismatch (expected {hash}, got {actual})"));
    }

    let dest = crate::import::managed_dest_for(music_dir, filename, got);
    mark_expected(&dest);
    // `rename` within the same filesystem is atomic: the file appears whole or
    // doesn't appear at all. It never ends up half-done in the library.
    std::fs::rename(&part, &dest)?;
    Ok(Received {
        path: dest,
        bytes: got,
    })
}

/// Requests a file and receives it, resuming if something was already downloaded.
pub fn pull_file(
    sess: &mut Session,
    music_dir: &Path,
    hash: &str,
    filename: &str,
    progress: &mut dyn FnMut(u64, u64),
    mark_expected: &mut dyn FnMut(&Path),
) -> Result<Received> {
    let offset = resume_offset(music_dir, hash);
    if offset > 0 {
        log::info!("[sync] resuming {filename} from {offset} bytes");
    }
    sess.send(&Msg::BlobReq {
        hash: hash.to_string(),
        offset,
    })?;
    receive_file(sess, music_dir, hash, filename, offset > 0, progress, mark_expected)
}

// ---------------------------------------------------------------------------
// Adding to the library
// ---------------------------------------------------------------------------

/// Adds a received file, keeping the `uid` from the other device.
///
/// The uid has to be the same on both sides or the next sync wouldn't
/// recognize it's the same song: playlists and tombstones reference it. This
/// is why the normal import path isn't used, since that generates a new one
/// and rereads the file's tags.
#[allow(clippy::too_many_arguments)]
pub fn insert_received(
    conn: &rusqlite::Connection,
    dest: &Path,
    uid: &str,
    hash: &str,
    size: u64,
    title: &str,
    artist: &str,
    album: &str,
    genre: &str,
    duration_ms: i64,
    bpm: Option<i64>,
    updated_at: i64,
) -> rusqlite::Result<i64> {
    let (_, mtime) = crate::hashing::file_stamp(dest).unwrap_or((0, 0));
    let rel = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // If there's already a row with this uid, another one CANNOT be inserted:
    // `uid` has a unique index and the INSERT blows up with "UNIQUE constraint
    // failed: tracks.uid". And that's not a cosmetic detail: the error kills
    // the whole session, so sync fails, retries on its own, and fails again —
    // nothing ever gets copied.
    //
    // This happens more often than it seems: while the hash backfill hasn't
    // set `content_hash` on a row yet, the other side doesn't see it as
    // present and sends the file that's already here.
    let existing: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, path FROM tracks WHERE uid = ?1",
            [uid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((id, old_path)) = existing {
        let old = Path::new(&old_path);
        if old != dest && old.exists() {
            // There was already a file for this track. We have to keep just
            // one —the uid is unique, the row points to one path— and above
            // all **record a hash that matches the file that remains**.
            //
            // Before this discarded the received copy without touching the
            // hash. If the local row had no hash (backfill pending), the
            // other side kept seeing that we were "missing" it and sent it
            // again: the same track traveling on every sync, forever.
            let same = crate::hashing::hash_file(old).map(|h| h == hash).unwrap_or(false);
            if same {
                // Same content: the transfer was redundant.
                let _ = std::fs::remove_file(dest);
                conn.execute(
                    "UPDATE tracks SET content_hash = ?1, size_bytes = ?2,
                            local_state = 'present' WHERE id = ?3",
                    rusqlite::params![hash, size as i64, id],
                )?;
                return Ok(id);
            }
            // Different content under the same track (different encoding,
            // different edit). The one that just arrived wins, and the one
            // that was there **goes to the library's trash**, not to the void:
            // it's still the user's music and is recoverable for 30 days.
            // Keeping the old one wouldn't work — the other side would send
            // its own again on every sync.
            // `dest` always hangs off the managed folder (`managed_dest_for`
            // chose it), so its parent IS the managed folder.
            if let Some(managed) = dest.parent() {
                match crate::trash::move_to_trash(managed, old) {
                    Ok(p) => {
                        log::info!("[sync] replaced, previous one moved to trash: {}", p.display())
                    }
                    Err(e) => log::warn!("[sync] could not store {}: {e}", old.display()),
                }
            }
        }
        // The row points to the file that just arrived: either it had none
        // (freed by selective sync, deleted by hand) or the one it had was
        // different content and was already archived above.
        conn.execute(
            "UPDATE tracks SET path = ?1, rel_path = ?2, content_hash = ?3,
                    size_bytes = ?4, mtime_ms = ?5, local_state = 'present'
             WHERE id = ?6",
            rusqlite::params![
                dest.to_string_lossy(),
                rel,
                hash,
                size as i64,
                mtime,
                id
            ],
        )?;
        return Ok(id);
    }

    conn.execute(
        "INSERT INTO tracks (path, title, artist, album, genre, duration_ms, bpm,
                             uid, content_hash, rel_path, size_bytes, mtime_ms,
                             updated_at, local_state)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 'present')
         ON CONFLICT(path) DO UPDATE SET
            content_hash = excluded.content_hash,
            size_bytes   = excluded.size_bytes,
            mtime_ms     = excluded.mtime_ms,
            local_state  = 'present'",
        rusqlite::params![
            dest.to_string_lossy(),
            title,
            artist,
            album,
            genre,
            duration_ms,
            bpm,
            uid,
            hash,
            rel,
            size as i64,
            mtime,
            updated_at
        ],
    )?;
    conn.query_row(
        "SELECT id FROM tracks WHERE path = ?1",
        [dest.to_string_lossy()],
        |r| r.get(0),
    )
}

/// Resolves the local file that corresponds to a hash, so it can be served.
pub fn path_for_hash(conn: &rusqlite::Connection, hash: &str) -> Option<PathBuf> {
    conn.query_row(
        "SELECT path FROM tracks WHERE content_hash = ?1 AND local_state = 'present' LIMIT 1",
        [hash],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .map(PathBuf::from)
    .filter(|p| p.exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::generate_keypair;
    use std::net::{TcpListener, TcpStream};

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sway-xfer-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Two ends of an encrypted session over loopback.
    fn pair() -> (Session, Session) {
        let (a, _) = generate_keypair().unwrap();
        let (b, _) = generate_keypair().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            Session::accept(s, &b).unwrap()
        });
        let client = Session::connect(TcpStream::connect(addr).unwrap(), &a).unwrap();
        (client, server.join().unwrap())
    }

    fn sample(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    /// A push sends the file from scratch: the other side doesn't know what
    /// we have half-downloaded. If the old `.part` were kept, the whole file
    /// would get stacked on top of it — longer, a different hash, and a
    /// complete transfer wasted that repeats on every sync.
    #[test]
    fn a_stale_partial_does_not_poison_a_push() {
        let src_dir = tmpdir("push-src");
        let dst_dir = tmpdir("push-dst");
        let src = src_dir.join("track.flac");
        let data = sample(CHUNK + 500);
        std::fs::write(&src, &data).unwrap();
        let hash = crate::hashing::hash_file(&src).unwrap();

        // A partial from a previous download that got cut off is left over.
        std::fs::create_dir_all(incoming_dir(&dst_dir)).unwrap();
        std::fs::write(part_path(&dst_dir, &hash), &data[..1000]).unwrap();

        let (mut client, mut server) = pair();
        let h = hash.clone();
        let src2 = src.clone();
        let sender = std::thread::spawn(move || {
            send_file(&mut server, &src2, 0, &h).unwrap();
        });
        let got = receive_file(
            &mut client,
            &dst_dir,
            &hash,
            "track.flac",
            false,
            &mut |_, _| {},
            &mut |_| {},
        )
        .expect("the old partial cannot ruin the push");
        sender.join().unwrap();

        assert_eq!(std::fs::read(&got.path).unwrap(), data);
        std::fs::remove_dir_all(&src_dir).ok();
        std::fs::remove_dir_all(&dst_dir).ok();
    }

    /// `uid` has a unique index: receiving a track that already exists here
    /// cannot end in "UNIQUE constraint failed: tracks.uid". That error used
    /// to kill the whole session, so sync would fail, retry on its own, and
    /// fail again — nothing ever got copied. It happens whenever the hash
    /// backfill hasn't yet set `content_hash` on the row: the other side
    /// doesn't see it as present and sends the file again.
    #[test]
    fn receiving_a_track_that_already_exists_here_does_not_break_the_session() {
        let dir = tmpdir("dup");
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::init_schema(&conn).unwrap();

        let mine = dir.join("i-already-have-it.flac");
        std::fs::write(&mine, b"audio").unwrap();
        conn.execute(
            "INSERT INTO tracks (path, uid, rel_path, local_state)
             VALUES (?1, 'uid-1', 'i-already-have-it.flac', 'present')",
            [mine.to_string_lossy()],
        )
        .unwrap();

        // The same content arrives with a different filename.
        let hash = crate::hashing::hash_file(&mine).unwrap();
        let incoming = dir.join("i-already-have-it (2).flac");
        std::fs::write(&incoming, b"audio").unwrap();
        let id = insert_received(&conn, &incoming, "uid-1", &hash, 5, "T", "A", "", "", 0, None, 10)
            .expect("cannot fail because of the unique index");

        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "a single row for that uid");
        let (path, stored): (String, Option<String>) = conn
            .query_row("SELECT path, content_hash FROM tracks WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(path, mine.to_string_lossy(), "keeps the file it already had");
        assert!(!incoming.exists(), "and the redundant copy isn't left lying around");
        // What closed the loop: without a recorded hash, the other side sees
        // us as "missing" it and sends it again on every sync.
        assert_eq!(stored.as_deref(), Some(hash.as_str()));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Same track (same uid) with different bytes on each side. We have to
    /// keep just one or sync never converges — but the loser **is not
    /// destroyed**: it goes to the library's trash.
    #[test]
    fn a_different_encoding_of_the_same_track_does_not_loop_and_does_not_lose_the_old_file() {
        let dir = tmpdir("replace");
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::init_schema(&conn).unwrap();

        let mine = dir.join("track.flac");
        std::fs::write(&mine, b"old version").unwrap();
        conn.execute(
            "INSERT INTO tracks (path, uid, rel_path, local_state)
             VALUES (?1, 'uid-1', 'track.flac', 'present')",
            [mine.to_string_lossy()],
        )
        .unwrap();

        let incoming = dir.join("track (2).flac");
        std::fs::write(&incoming, b"new version").unwrap();
        let hash = crate::hashing::hash_file(&incoming).unwrap();
        let id = insert_received(&conn, &incoming, "uid-1", &hash, 13, "T", "A", "", "", 0, None, 10)
            .unwrap();

        let (path, stored): (String, Option<String>) = conn
            .query_row("SELECT path, content_hash FROM tracks WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(path, incoming.to_string_lossy(), "the one that arrived wins");
        assert_eq!(stored.as_deref(), Some(hash.as_str()), "and its hash gets recorded");
        assert!(!mine.exists(), "the old one leaves the library");
        let recoverable = std::fs::read_dir(crate::trash::trash_dir(&dir))
            .unwrap()
            .flatten()
            .any(|e| std::fs::read(e.path()).unwrap() == b"old version");
        assert!(recoverable, "but it still exists in the trash");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Full cycle of selective sync: space is freed (the row becomes
    /// `absent`) and when the playlist is re-marked, the file comes back
    /// **to the same row**. Reinserting with the same uid and a different
    /// path used to collide with `uid`'s unique index, and the track came
    /// back duplicated or didn't come back at all.
    #[test]
    fn a_freed_track_comes_back_to_the_same_row() {
        let dir = tmpdir("recover");
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::init_schema(&conn).unwrap();

        let first = dir.join("track.flac");
        std::fs::write(&first, b"audio").unwrap();
        insert_received(&conn, &first, "uid-1", "h1", 5, "T", "A", "", "", 0, None, 10).unwrap();

        // Freed: the row stays, the file doesn't.
        conn.execute(
            "UPDATE tracks SET local_state = 'absent' WHERE uid = 'uid-1'",
            [],
        )
        .unwrap();
        std::fs::remove_file(&first).unwrap();

        // It comes back, with a different name too (the destination is
        // disambiguated if needed).
        let again = dir.join("track (2).flac");
        std::fs::write(&again, b"audio").unwrap();
        insert_received(&conn, &again, "uid-1", "h1", 5, "T", "A", "", "", 0, None, 10).unwrap();

        let (n, state, path): (i64, String, String) = conn
            .query_row(
                "SELECT COUNT(*), MAX(local_state), MAX(path) FROM tracks WHERE uid = 'uid-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(n, 1, "a single row, not a duplicate");
        assert_eq!(state, "present");
        assert_eq!(path, again.to_string_lossy());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn transfers_and_verifies_a_file() {
        let src_dir = tmpdir("src");
        let dst_dir = tmpdir("dst");
        let src = src_dir.join("track.flac");
        // More than one chunk, to exercise the chunking.
        let data = sample(CHUNK * 2 + 1234);
        std::fs::write(&src, &data).unwrap();
        let hash = crate::hashing::hash_file(&src).unwrap();

        let (mut client, mut server) = pair();
        let h = hash.clone();
        let sender = std::thread::spawn(move || {
            match server.recv().unwrap() {
                Msg::BlobReq { offset, .. } => {
                    send_file(&mut server, &src, offset, &h).unwrap();
                }
                other => panic!("unexpected: {other:?}"),
            }
        });

        let got = pull_file(&mut client, &dst_dir, &hash, "track.flac", &mut |_, _| {}, &mut |_| {}).unwrap();
        sender.join().unwrap();

        assert_eq!(std::fs::read(&got.path).unwrap(), data);
        assert_eq!(got.bytes as usize, data.len());
        // The .part was consumed: no leftover junk.
        assert!(!part_path(&dst_dir, &hash).exists());
        std::fs::remove_dir_all(&src_dir).ok();
        std::fs::remove_dir_all(&dst_dir).ok();
    }

    /// A sender that sends bytes that aren't the ones for the requested
    /// hash —bug, corruption in transit, or bad faith— can't get anything
    /// into the library. It's the only check that matters: the hash declared
    /// in BlobEnd proves nothing, what's verified is the bytes received.
    #[test]
    fn content_that_does_not_match_the_requested_hash_is_rejected() {
        let src_dir = tmpdir("bad-src");
        let dst_dir = tmpdir("bad-dst");
        // What the receiver wants...
        let wanted = src_dir.join("good.flac");
        std::fs::write(&wanted, sample(5000)).unwrap();
        let wanted_hash = crate::hashing::hash_file(&wanted).unwrap();
        // ...and what the sender sends instead, claiming it's the same.
        let other = src_dir.join("other.flac");
        std::fs::write(&other, sample(4096)).unwrap();

        let (mut client, mut server) = pair();
        let claimed = wanted_hash.clone();
        let sender = std::thread::spawn(move || {
            let offset = match server.recv().unwrap() {
                Msg::BlobReq { offset, .. } => offset,
                other => panic!("unexpected: {other:?}"),
            };
            send_file(&mut server, &other, offset, &claimed).unwrap();
        });

        let err = pull_file(&mut client, &dst_dir, &wanted_hash, "good.flac", &mut |_, _| {}, &mut |_| {})
            .unwrap_err();
        sender.join().unwrap();

        assert!(err.to_string().contains("hash mismatch"), "unexpected error: {err}");
        // Neither the final file nor the .part: the library stayed intact.
        assert!(!dst_dir.join("good.flac").exists());
        assert!(!part_path(&dst_dir, &wanted_hash).exists());
        std::fs::remove_dir_all(&src_dir).ok();
        std::fs::remove_dir_all(&dst_dir).ok();
    }

    /// And if it also lies in BlobEnd, it fails even earlier.
    #[test]
    fn a_wrong_hash_in_blob_end_is_rejected() {
        let src_dir = tmpdir("end-src");
        let dst_dir = tmpdir("end-dst");
        let src = src_dir.join("x.flac");
        std::fs::write(&src, sample(2000)).unwrap();
        let hash = crate::hashing::hash_file(&src).unwrap();

        let (mut client, mut server) = pair();
        let sender = std::thread::spawn(move || {
            let offset = match server.recv().unwrap() {
                Msg::BlobReq { offset, .. } => offset,
                other => panic!("unexpected: {other:?}"),
            };
            send_file(&mut server, &src, offset, &"0".repeat(64)).unwrap();
        });
        let err = pull_file(&mut client, &dst_dir, &hash, "x.flac", &mut |_, _| {}, &mut |_| {}).unwrap_err();
        sender.join().unwrap();

        assert!(err.to_string().contains("reports having sent"), "error: {err}");
        assert!(!part_path(&dst_dir, &hash).exists());
        std::fs::remove_dir_all(&src_dir).ok();
        std::fs::remove_dir_all(&dst_dir).ok();
    }

    /// A transfer cut off halfway leaves a .part, and the next one starts
    /// from there instead of downloading everything again.
    #[test]
    fn an_interrupted_transfer_resumes_from_where_it_stopped() {
        let src_dir = tmpdir("res-src");
        let dst_dir = tmpdir("res-dst");
        let src = src_dir.join("long.flac");
        let data = sample(CHUNK * 3);
        std::fs::write(&src, &data).unwrap();
        let hash = crate::hashing::hash_file(&src).unwrap();

        // Simulates the cutoff: half the file is already downloaded from a
        // previous run.
        std::fs::create_dir_all(incoming_dir(&dst_dir)).unwrap();
        let half = data.len() / 2;
        std::fs::write(part_path(&dst_dir, &hash), &data[..half]).unwrap();

        let (mut client, mut server) = pair();
        let h = hash.clone();
        let asked = std::thread::spawn(move || {
            let offset = match server.recv().unwrap() {
                Msg::BlobReq { offset, .. } => offset,
                other => panic!("unexpected: {other:?}"),
            };
            send_file(&mut server, &src, offset, &h).unwrap();
            offset
        });

        let got = pull_file(&mut client, &dst_dir, &hash, "long.flac", &mut |_, _| {}, &mut |_| {}).unwrap();
        let offset = asked.join().unwrap();

        assert_eq!(offset, half as u64, "had to request only what was missing");
        // And the reassembled file is identical to the original.
        assert_eq!(std::fs::read(&got.path).unwrap(), data);
        std::fs::remove_dir_all(&src_dir).ok();
        std::fs::remove_dir_all(&dst_dir).ok();
    }

    /// A .part with a corrupt prefix (failed disk, dirty cutoff) can't be
    /// detected by size alone: it only fails when the whole file is verified.
    #[test]
    fn a_corrupted_resume_prefix_is_caught_by_the_full_hash() {
        let src_dir = tmpdir("pre-src");
        let dst_dir = tmpdir("pre-dst");
        let src = src_dir.join("x.flac");
        let data = sample(CHUNK + 500);
        std::fs::write(&src, &data).unwrap();
        let hash = crate::hashing::hash_file(&src).unwrap();

        let half = data.len() / 2;
        let mut bad = data[..half].to_vec();
        bad[10] ^= 0xFF; // one byte changed, same size
        std::fs::create_dir_all(incoming_dir(&dst_dir)).unwrap();
        std::fs::write(part_path(&dst_dir, &hash), &bad).unwrap();

        let (mut client, mut server) = pair();
        let h = hash.clone();
        let sender = std::thread::spawn(move || {
            let offset = match server.recv().unwrap() {
                Msg::BlobReq { offset, .. } => offset,
                other => panic!("unexpected: {other:?}"),
            };
            send_file(&mut server, &src, offset, &h).unwrap();
        });

        let err =
            pull_file(&mut client, &dst_dir, &hash, "x.flac", &mut |_, _| {}, &mut |_| {}).unwrap_err();
        sender.join().unwrap();
        assert!(err.to_string().contains("hash"), "unexpected error: {err}");
        assert!(!part_path(&dst_dir, &hash).exists(), "the bad .part is discarded");
        std::fs::remove_dir_all(&src_dir).ok();
        std::fs::remove_dir_all(&dst_dir).ok();
    }

    /// An existing file is never overwritten: if the name is taken by
    /// different content, the incoming one is disambiguated.
    #[test]
    fn an_existing_file_with_the_same_name_is_never_overwritten() {
        let src_dir = tmpdir("dup-src");
        let dst_dir = tmpdir("dup-dst");
        let existing = dst_dir.join("same.flac");
        std::fs::write(&existing, b"what was already there").unwrap();

        let src = src_dir.join("same.flac");
        let data = sample(3000);
        std::fs::write(&src, &data).unwrap();
        let hash = crate::hashing::hash_file(&src).unwrap();

        let (mut client, mut server) = pair();
        let h = hash.clone();
        let sender = std::thread::spawn(move || {
            let offset = match server.recv().unwrap() {
                Msg::BlobReq { offset, .. } => offset,
                other => panic!("unexpected: {other:?}"),
            };
            send_file(&mut server, &src, offset, &h).unwrap();
        });
        let got = pull_file(&mut client, &dst_dir, &hash, "same.flac", &mut |_, _| {}, &mut |_| {}).unwrap();
        sender.join().unwrap();

        assert_ne!(got.path, existing);
        assert_eq!(std::fs::read(&existing).unwrap(), b"what was already there");
        assert_eq!(std::fs::read(&got.path).unwrap(), data);
        std::fs::remove_dir_all(&src_dir).ok();
        std::fs::remove_dir_all(&dst_dir).ok();
    }
}
