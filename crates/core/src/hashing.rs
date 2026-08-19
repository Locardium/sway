//! Content hash (blake3) of the library files.
//!
//! The hash is the identity of the BYTES: it's what gets requested in a
//! transfer, what gets verified when it finishes, and what allows detecting
//! that two devices already have the same file even if they imported it
//! separately under different names.
//!
//! Constraints that shape this module:
//! - Libraries of 100+ GB. Never read an entire file into memory, and the
//!   initial backfill can't block the UI.
//! - Rehashing on every startup would be unacceptable: `(size, mtime)` acts as
//!   a cache. If neither changed, the stored hash is still valid.

use rusqlite::Connection;
use std::io::Read;
use std::path::Path;

const CHUNK: usize = 256 * 1024;

/// blake3 of the file, read in chunks.
pub fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// `(size, mtime in ms)` of the file.
pub fn file_stamp(path: &Path) -> std::io::Result<(i64, i64)> {
    let md = std::fs::metadata(path)?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Ok((md.len() as i64, mtime))
}

/// Hashes a track and stores hash + stamp. No-op if the stored stamp matches
/// the file's (nothing changed since last time).
///
/// The startup backfill (`spawn_hash_backfill` in lib.rs) intentionally
/// doesn't use it: it needs to compute the hash OUTSIDE the DB lock so it
/// doesn't freeze the UI. This is the single-track path, and it's the one
/// the post-transfer verification in 5.4 will use.
#[allow(dead_code)]
pub fn hash_track(conn: &Connection, id: i64, path: &Path) -> anyhow::Result<Option<String>> {
    let (size, mtime) = match file_stamp(path) {
        Ok(v) => v,
        // Missing file: not a fatal error — could be a legacy track outside
        // the managed folder, or a blob not yet transferred.
        Err(_) => return Ok(None),
    };
    let current: (Option<String>, Option<i64>, Option<i64>) = conn.query_row(
        "SELECT content_hash, size_bytes, mtime_ms FROM tracks WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if let (Some(h), Some(s), Some(m)) = current {
        if s == size && m == mtime {
            return Ok(Some(h));
        }
    }
    let hash = hash_file(path)?;
    conn.execute(
        "UPDATE tracks SET content_hash = ?1, size_bytes = ?2, mtime_ms = ?3 WHERE id = ?4",
        rusqlite::params![hash, size, mtime, id],
    )?;
    Ok(Some(hash))
}

/// Tracks that don't yet have a valid hash (never hashed, or the file's
/// size/date changed since last time).
pub fn pending(conn: &Connection) -> rusqlite::Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, path FROM tracks
         WHERE content_hash IS NULL OR size_bytes IS NULL OR mtime_ms IS NULL",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_content_dependent() {
        let dir = std::env::temp_dir().join(format!("sway-hash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.bin");
        let b = dir.join("b.bin");
        // Larger than a chunk, to exercise the read loop.
        let data: Vec<u8> = (0..(CHUNK * 2 + 7)).map(|i| (i % 251) as u8).collect();
        std::fs::write(&a, &data).unwrap();
        std::fs::write(&b, &data).unwrap();
        assert_eq!(hash_file(&a).unwrap(), hash_file(&b).unwrap());
        assert_eq!(hash_file(&a).unwrap(), hash_file(&a).unwrap());

        let mut changed = data.clone();
        *changed.last_mut().unwrap() ^= 0xFF;
        std::fs::write(&b, &changed).unwrap();
        assert_ne!(hash_file(&a).unwrap(), hash_file(&b).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hash_track_caches_by_stamp() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE tracks (id INTEGER PRIMARY KEY, path TEXT, content_hash TEXT,
                size_bytes INTEGER, mtime_ms INTEGER);",
        )
        .unwrap();
        let dir = std::env::temp_dir().join(format!("sway-stamp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("t.bin");
        std::fs::write(&f, b"hello").unwrap();
        conn.execute("INSERT INTO tracks (id, path) VALUES (1, ?1)", [f.to_str().unwrap()])
            .unwrap();

        let first = hash_track(&conn, 1, &f).unwrap().unwrap();
        assert!(pending(&conn).unwrap().is_empty());

        // Same stamp -> returns the stored value without rereading.
        assert_eq!(hash_track(&conn, 1, &f).unwrap().unwrap(), first);

        // Different content -> different stamp -> rehashes.
        std::fs::write(&f, b"totally different content").unwrap();
        let second = hash_track(&conn, 1, &f).unwrap().unwrap();
        assert_ne!(first, second);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE tracks (id INTEGER PRIMARY KEY, path TEXT, content_hash TEXT,
                size_bytes INTEGER, mtime_ms INTEGER);
             INSERT INTO tracks (id, path) VALUES (1, '/no/exists.flac');",
        )
        .unwrap();
        assert!(hash_track(&conn, 1, Path::new("/no/exists.flac")).unwrap().is_none());
    }
}
