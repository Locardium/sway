use crate::id3_sanitize::sanitize_id3v2_date_frames;
use anyhow::Result;
use lofty::config::ParseOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::prelude::{Accessor, ItemKey};
use lofty::probe::Probe;
use rusqlite::{params, Connection};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const AUDIO_EXTS: &[&str] = &[
    "mp3", "flac", "wav", "m4a", "aac", "aif", "aiff", "ogg", "opus",
];

struct Meta {
    title: String,
    artist: String,
    album: String,
    genre: String,
    duration_ms: i64,
    bpm: Option<i64>,
}

fn read_meta(path: &Path) -> Meta {
    let fallback = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let mut m = Meta {
        title: fallback,
        artist: String::new(),
        album: String::new(),
        genre: String::new(),
        duration_ms: 0,
        bpm: None,
    };
    match lofty::read_from_path(path) {
        Ok(tagged) => fill_meta_from_tagged(&mut m, &tagged),
        Err(e) => {
            log::warn!("lofty read_from_path (with tags) failed for {}: {e}", path.display());
            apply_meta_from_broken_tag(path, &mut m);
        }
    }
    m
}

/// Called when the normal lofty pass fails. An individually corrupt tag
/// frame (e.g. `TORY`/`TYER` with an invalid date — seen in practice with
/// text from a download site overwriting the field) makes lofty discard the
/// ENTIRE tag, even though the rest is perfectly readable. First it tries to
/// sanitize those frames and reparse in memory (rescues everything:
/// title/artist/cover/duration); if that doesn't apply or still isn't
/// enough, a last pass without parsing tags rescues at least the real
/// duration (critical for the seek bar) instead of leaving it at 0. The
/// title already falls back to the file name.
fn apply_meta_from_broken_tag(path: &Path, m: &mut Meta) {
    if let Ok(bytes) = std::fs::read(path) {
        if let Some(patched) = sanitize_id3v2_date_frames(&bytes) {
            let parsed = Probe::new(Cursor::new(patched.as_slice()))
                .guess_file_type()
                .ok()
                .and_then(|p| p.read().ok());
            if let Some(tagged) = parsed {
                log::info!("id3_sanitize: {} recovered its full tag after sanitizing", path.display());
                fill_meta_from_tagged(m, &tagged);
                return;
            }
            log::warn!("id3_sanitize: {} still does not parse even after sanitizing", path.display());
        }
    }
    let props_only = ParseOptions::new().read_tags(false);
    match Probe::open(path).and_then(|p| p.options(props_only).read()) {
        Ok(tagged) => {
            m.duration_ms = tagged.properties().duration().as_millis() as i64;
        }
        Err(e2) => {
            log::warn!("lofty properties-only also failed for {}: {e2}", path.display());
        }
    }
}

fn fill_meta_from_tagged(m: &mut Meta, tagged: &lofty::file::TaggedFile) {
    m.duration_ms = tagged.properties().duration().as_millis() as i64;
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        if let Some(t) = tag.title() {
            if !t.is_empty() {
                m.title = t.to_string();
            }
        }
        if let Some(a) = tag.artist() {
            m.artist = a.to_string();
        }
        if let Some(al) = tag.album() {
            m.album = al.to_string();
        }
        if let Some(g) = tag.genre() {
            m.genre = g.to_string();
        }
        if let Some(b) = tag.get_string(ItemKey::IntegerBpm) {
            m.bpm = b.parse().ok();
        }
    }
}

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase())
        .is_some_and(|ext| AUDIO_EXTS.contains(&ext.as_str()))
}

/// Destination inside the managed folder. Reuses the file if one already
/// exists with the same name and size; otherwise disambiguates with " (n)".
fn managed_dest(managed: &Path, src: &Path) -> PathBuf {
    let name = src.file_name().unwrap_or_default();
    let dest = managed.join(name);
    if dest.exists() {
        let ssize = std::fs::metadata(src).ok().map(|m| m.len());
        let dsize = std::fs::metadata(&dest).ok().map(|m| m.len());
        if ssize.is_some() && ssize == dsize {
            return dest; // same file, reuse
        }
        let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("track");
        let ext = src.extension().and_then(|s| s.to_str()).unwrap_or("");
        let mut i = 2;
        loop {
            let cand = if ext.is_empty() {
                managed.join(format!("{stem} ({i})"))
            } else {
                managed.join(format!("{stem} ({i}).{ext}"))
            };
            if !cand.exists() {
                return cand;
            }
            i += 1;
        }
    }
    dest
}

/// Inserts the track already located at `dest` inside the managed folder, or
/// refreshes its tags if the row already exists (the path is UNIQUE). Shared
/// by `import_one` (src on disk) and `import_bytes` (src in memory, see
/// below).
///
/// The refresh matters: reimporting a file that was already in the library
/// is the only way the user has to recover tags that an old import failed to
/// read (e.g. tracks imported before the `id3_sanitize` fix). With
/// `INSERT OR IGNORE` the reimport was a silent no-op and the row was left
/// with empty fields forever. Each field is only overwritten if the new read
/// brings something — a worse read (unreadable tag) never erases good data.
fn insert_track(conn: &Connection, dest: &Path) -> Result<i64> {
    let m = read_meta(dest);
    // Identity for sync (Phase 5). The hash is computed here rather than in
    // the startup backfill because the file has just been read whole to copy
    // it: the marginal cost is zero and the track is syncable from the start.
    let (size, mtime) = crate::hashing::file_stamp(dest).unwrap_or((0, 0));
    let hash = crate::hashing::hash_file(dest).ok();
    let rel = dest.file_name().map(|n| n.to_string_lossy().into_owned());
    conn.execute(
        "INSERT INTO tracks (path, title, artist, album, genre, duration_ms, bpm,
                             uid, content_hash, rel_path, size_bytes, mtime_ms, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(path) DO UPDATE SET
            content_hash = excluded.content_hash,
            size_bytes   = excluded.size_bytes,
            mtime_ms     = excluded.mtime_ms,
            rel_path     = COALESCE(tracks.rel_path, excluded.rel_path),
            -- Only counts as a change if the BYTES changed. Reimporting the
            -- same file (the watcher fires several events per file, and sync
            -- leaves new files in the watched folder) is not an edit: if it
            -- bumped the date, this device would look newer every round and
            -- the other one would keep re-applying the same metadata
            -- forever, never converging.
            updated_at   = CASE WHEN excluded.content_hash IS NOT tracks.content_hash
                                THEN excluded.updated_at ELSE tracks.updated_at END,
            title       = CASE WHEN excluded.title  <> '' THEN excluded.title  ELSE tracks.title  END,
            artist      = CASE WHEN excluded.artist <> '' THEN excluded.artist ELSE tracks.artist END,
            album       = CASE WHEN excluded.album  <> '' THEN excluded.album  ELSE tracks.album  END,
            genre       = CASE WHEN excluded.genre  <> '' THEN excluded.genre  ELSE tracks.genre  END,
            duration_ms = CASE WHEN excluded.duration_ms > 0 THEN excluded.duration_ms ELSE tracks.duration_ms END,
            bpm         = COALESCE(excluded.bpm, tracks.bpm)",
        params![
            dest.to_string_lossy(),
            m.title,
            m.artist,
            m.album,
            m.genre,
            m.duration_ms,
            m.bpm,
            // The uid is NOT overwritten on UPDATE: reimporting a file that
            // was already there is the same song, and its logical identity
            // has to survive (playlists and tombstones reference it).
            crate::db::new_uid(),
            hash,
            rel,
            size,
            mtime,
            crate::db::now_ms()
        ],
    )?;
    conn.query_row(
        "SELECT id FROM tracks WHERE path = ?1",
        [dest.to_string_lossy()],
        |r| r.get(0),
    )
    .map_err(Into::into)
}

/// Copies the file to the managed folder (if needed), reads tags and
/// inserts. Returns the id. Tracks always end up under `managed`.
fn import_one(conn: &Connection, managed: &Path, src: &Path) -> Result<i64> {
    let already_managed = src.starts_with(managed);
    let dest = if already_managed {
        src.to_path_buf()
    } else {
        managed_dest(managed, src)
    };
    if dest != src && !dest.exists() {
        std::fs::copy(src, &dest)?;
    }
    insert_track(conn, &dest)
}

/// Same as `managed_dest` but without a source file on disk to stat (the
/// bytes are already in memory — see `import_bytes`).
pub fn managed_dest_for(managed: &Path, name: &str, size: u64) -> PathBuf {
    let dest = managed.join(name);
    if dest.exists() {
        let dsize = std::fs::metadata(&dest).ok().map(|m| m.len());
        if dsize == Some(size) {
            return dest; // same file, reuse
        }
        let stem = Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or("track");
        let ext = Path::new(name).extension().and_then(|s| s.to_str()).unwrap_or("");
        let mut i = 2;
        loop {
            let cand = if ext.is_empty() {
                managed.join(format!("{stem} ({i})"))
            } else {
                managed.join(format!("{stem} ({i}).{ext}"))
            };
            if !cand.exists() {
                return cand;
            }
            i += 1;
        }
    }
    dest
}

/// Imports bytes already read into memory (Android: the picker gives
/// `content://` URIs that Rust can't open with `std::fs`; the
/// `import_from_uri` command in lib.rs resolves them via `tauri_plugin_fs`
/// and sends bytes + original name here). Doesn't validate against
/// `AUDIO_EXTS`: the picker already filtered by MIME type on the OS side,
/// and lofty detects the real format by content even if the name comes with
/// a generic extension.
pub fn import_bytes(conn: &Connection, managed: &Path, name: &str, bytes: &[u8]) -> Result<i64> {
    let dest = managed_dest_for(managed, name, bytes.len() as u64);
    if !dest.exists() {
        std::fs::write(&dest, bytes)?;
    }
    insert_track(conn, &dest)
}

/// Internal app directories inside the managed folder. NOTHING inside these
/// gets imported.
///
/// `.sway-trash` is the case that matters: it keeps the file's original name
/// and extension, so without this exclusion the watcher sees a .flac appear,
/// auto-imports it, and what you just deleted reappears in the library with
/// a new uid — and on the next sync you send it to the other device. A
/// delete that undoes itself.
pub fn is_internal_dir(name: &str) -> bool {
    name.starts_with(".sway-")
}

/// True if the path is inside an internal app directory.
pub fn is_internal_path(path: &Path) -> bool {
    path.components().any(|c| match c {
        std::path::Component::Normal(n) => {
            n.to_str().map(is_internal_dir).unwrap_or(false)
        }
        _ => false,
    })
}

/// Gathers all audio files under `roots` (expands directories).
fn collect_audio(roots: &[&Path]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for root in roots {
        if root.is_dir() {
            let walk = WalkDir::new(root)
                .into_iter()
                .filter_entry(|e| {
                    !e.file_name()
                        .to_str()
                        .map(is_internal_dir)
                        .unwrap_or(false)
                });
            for entry in walk.filter_map(|e| e.ok()) {
                if entry.file_type().is_file() && is_audio(entry.path()) {
                    files.push(entry.path().to_path_buf());
                }
            }
        } else if root.is_file() && is_audio(root) && !is_internal_path(root) {
            files.push(root.to_path_buf());
        }
    }
    files
}

/// Scans `folder` recursively, copies to the managed folder and inserts the
/// new ones. `progress(done, total)` is called for each file processed.
pub fn import_folder(
    conn: &Connection,
    managed: &Path,
    folder: &str,
    progress: impl Fn(usize, usize),
) -> Result<usize> {
    let before: i64 = conn.query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))?;
    let files = collect_audio(&[Path::new(folder)]);
    let total = files.len();
    for (i, f) in files.iter().enumerate() {
        import_one(conn, managed, f)?;
        progress(i + 1, total);
    }
    let after: i64 = conn.query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))?;
    Ok((after - before) as usize)
}

/// Imports a mix of files and folders (drop from the OS). Returns the ids of
/// all tracks involved. `progress(done, total)` per file.
pub fn import_paths(
    conn: &Connection,
    managed: &Path,
    paths: &[String],
    progress: impl Fn(usize, usize),
) -> Result<Vec<i64>> {
    let roots: Vec<&Path> = paths.iter().map(Path::new).collect();
    let files = collect_audio(&roots);
    let total = files.len();
    let mut ids = Vec::with_capacity(total);
    for (i, f) in files.iter().enumerate() {
        ids.push(import_one(conn, managed, f)?);
        progress(i + 1, total);
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reimporting the same file must never count as an edit.
    ///
    /// If it bumped `updated_at`, this device would look newer than the
    /// other one every round and sync would keep re-applying the same
    /// metadata forever, never converging. This actually happened: the
    /// watcher would re-import what sync left behind and every sync reported
    /// the same "5 meta".
    #[test]
    fn reimporting_the_same_file_does_not_look_like_an_edit() {
        let dir = std::env::temp_dir().join(format!("sway-reimp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("track.mp3");
        std::fs::write(&f, b"some bytes").unwrap();

        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_schema(&conn).unwrap();

        insert_track(&conn, &f).unwrap();
        let first: i64 = conn
            .query_row("SELECT updated_at FROM tracks", [], |r| r.get(0))
            .unwrap();
        assert!(first > 0);

        std::thread::sleep(std::time::Duration::from_millis(5));
        insert_track(&conn, &f).unwrap();
        let second: i64 = conn
            .query_row("SELECT updated_at FROM tracks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(first, second, "reimporting the same file is not a change");

        // But if the bytes change, it is.
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(&f, b"different bytes now").unwrap();
        insert_track(&conn, &f).unwrap();
        let third: i64 = conn
            .query_row("SELECT updated_at FROM tracks", [], |r| r.get(0))
            .unwrap();
        assert!(third > second, "a different file is indeed a change");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The trash lives INSIDE the managed folder and keeps the file's
    /// original name and extension. Without excluding it, the watcher sees a
    /// .flac appear there, auto-imports it, and what you just deleted comes
    /// back into the library with a new uid — and on the next sync you send
    /// it to the other device. A delete that undoes itself.
    #[test]
    fn internal_folders_are_never_imported() {
        let root = std::env::temp_dir().join(format!("sway-imp-{}", std::process::id()));
        let trash = root.join(".sway-trash");
        let incoming = root.join(".sway-incoming");
        std::fs::create_dir_all(&trash).unwrap();
        std::fs::create_dir_all(&incoming).unwrap();
        std::fs::write(root.join("normal.flac"), b"x").unwrap();
        std::fs::write(trash.join("deleted.flac"), b"x").unwrap();
        std::fs::write(incoming.join("downloading.flac"), b"x").unwrap();

        let found = collect_audio(&[root.as_path()]);
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["normal.flac"]);

        // It also doesn't get in if the path is passed loose (dropping a single file).
        assert!(collect_audio(&[trash.join("deleted.flac").as_path()]).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }
}
