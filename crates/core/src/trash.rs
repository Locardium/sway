//! The library's own trash.
//!
//! A deletion that arrives via sync doesn't destroy bytes. The file gets
//! moved to `<sway>/trash/` and stays there for a long while before
//! actually disappearing.
//!
//! The reason is the hard requirement of all of Phase 5: never lose music. A
//! local deletion you did yourself while looking at the screen; one that
//! arrives over the network could come from an out-of-sync device, an old
//! tombstone, or a bug. The trash is what makes that case recoverable instead
//! of final.

use crate::internal_dir::internal_base;
use std::path::{Path, PathBuf};

/// How long a deleted file survives before it's actually cleaned up.
pub const RETENTION_DAYS: u64 = 30;

/// `music_dir` is the folder tracks are scanned from (`<sway>/library` on
/// desktop). The trash sits one level up, next to the db, so it never gets
/// mistaken for a track by the scanner.
pub fn trash_dir(music_dir: &Path) -> PathBuf {
    internal_base(music_dir).join("trash")
}

/// Sends a file to the trash. Returns where it ended up.
///
/// Never overwrites: if something with that name already exists (you deleted
/// two files with the same name), it disambiguates.
pub fn move_to_trash(music_dir: &Path, path: &Path) -> std::io::Result<PathBuf> {
    let dir = trash_dir(music_dir);
    std::fs::create_dir_all(&dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "track".into());
    let mut dest = dir.join(&name);
    let mut i = 2;
    while dest.exists() {
        let stem = Path::new(&name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("track");
        let ext = Path::new(&name).extension().and_then(|s| s.to_str()).unwrap_or("");
        dest = if ext.is_empty() {
            dir.join(format!("{stem} ({i})"))
        } else {
            dir.join(format!("{stem} ({i}).{ext}"))
        };
        i += 1;
    }
    // `rename` is cheap and atomic; the fallback covers the case where the
    // trash ends up on a different volume than the file.
    match std::fs::rename(path, &dest) {
        Ok(()) => Ok(dest),
        Err(_) => {
            std::fs::copy(path, &dest)?;
            std::fs::remove_file(path)?;
            Ok(dest)
        }
    }
}

/// Actually deletes what has already met the retention period. Runs at startup.
pub fn purge_old(music_dir: &Path, retention_days: u64) -> usize {
    let dir = trash_dir(music_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(retention_days * 24 * 3600));
    let Some(cutoff) = cutoff else { return 0 };
    let mut removed = 0;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        // With no readable date, it's kept: when in doubt, don't delete.
        let Ok(modified) = meta.modified() else { continue };
        if modified < cutoff && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        log::info!("[trash] {removed} file(s) purged after {retention_days} days");
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sway-trash-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn moves_the_file_instead_of_destroying_it() {
        let music = tmpdir("move");
        let f = music.join("track.flac");
        std::fs::write(&f, b"audio").unwrap();

        let dest = move_to_trash(&music, &f).unwrap();
        assert!(!f.exists(), "leaves the library");
        assert!(dest.exists(), "but still exists");
        assert_eq!(std::fs::read(&dest).unwrap(), b"audio");
        std::fs::remove_dir_all(&music).ok();
    }

    /// Two different files that had the same name both have to survive:
    /// the trash can't overwrite what it already rescued.
    #[test]
    fn a_name_collision_in_the_trash_does_not_overwrite() {
        let music = tmpdir("collide");
        for content in [b"first".as_slice(), b"second".as_slice()] {
            let f = music.join("same.flac");
            std::fs::write(&f, content).unwrap();
            move_to_trash(&music, &f).unwrap();
        }
        let files: Vec<_> = std::fs::read_dir(trash_dir(&music))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(files.len(), 2);
        std::fs::remove_dir_all(&music).ok();
    }

    #[test]
    fn purge_keeps_recent_files() {
        let music = tmpdir("purge");
        let f = music.join("new.flac");
        std::fs::write(&f, b"x").unwrap();
        move_to_trash(&music, &f).unwrap();

        // Just deleted: retention protects it.
        assert_eq!(purge_old(&music, RETENTION_DAYS), 0);
        // With zero retention, it goes.
        assert_eq!(purge_old(&music, 0), 1);
        std::fs::remove_dir_all(&music).ok();
    }
}
