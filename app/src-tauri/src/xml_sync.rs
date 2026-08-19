//! OS location + backup + writing of the iTunes Music Library.xml. Everything
//! that knows about Windows paths and "who owns this file" lives here,
//! separate from the pure generator in `export_xml`.

use crate::db;
use crate::export_xml;
use anyhow::Result;
#[cfg(target_os = "windows")]
use anyhow::Context;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tauri::AppHandle;
#[cfg(target_os = "windows")]
use tauri::Manager;

/// Key that `export_xml::generate_xml` puts into every file Sway writes.
/// Used to distinguish "we wrote this" (no backup needed) from "this is
/// foreign" (the user's original library, or the file rewritten by real
/// iTunes/Music since the last time we wrote it).
const MARKER: &str = "<key>Sway Generator</key>";

/// Standard location of the iTunes library on Windows
/// (`<Music>\iTunes\iTunes Music Library.xml`). Windows only for now:
/// Phase 0 validated the format there; the modern Mac Music.app requires the
/// user to manually enable "Share Library XML" and the path there hasn't
/// been validated.
#[cfg(target_os = "windows")]
pub fn itunes_library_path(app: &AppHandle) -> Result<PathBuf> {
    let dir = app
        .path()
        .audio_dir()
        .context("could not resolve the Music folder")?
        .join("iTunes");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("iTunes Music Library.xml"))
}

#[cfg(not(target_os = "windows"))]
pub fn itunes_library_path(_app: &AppHandle) -> Result<PathBuf> {
    anyhow::bail!("auto-sync to iTunes is only supported on Windows for now")
}

/// If `target` exists and does NOT have our marker, renames it in the SAME
/// path (same folder as the original) before we overwrite it. If it has the
/// marker (we wrote it last time) does nothing — backing up our own file
/// makes no sense.
fn backup_if_foreign(target: &Path) -> Result<()> {
    if !target.exists() {
        return Ok(());
    }
    let contents = std::fs::read_to_string(target).unwrap_or_default();
    if contents.contains(MARKER) {
        return Ok(());
    }
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let stem = target
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("iTunes Music Library");
    let ext = target.extension().and_then(|s| s.to_str()).unwrap_or("xml");
    // First time: stable "original" name. If it already exists (real iTunes
    // wrote the file again afterward), a timestamp so it isn't overwritten.
    let preferred = dir.join(format!("{stem}.original.{ext}"));
    let backup_path = if !preferred.exists() {
        preferred
    } else {
        let ts = export_xml::compact_timestamp(SystemTime::now());
        dir.join(format!("{stem}.bak-{ts}.{ext}"))
    };
    std::fs::rename(target, &backup_path)?;
    Ok(())
}

/// Generates + backs up if needed + writes. Shared path used by the manual
/// "Sync now" button and by auto-sync.
pub fn write_now(app: &AppHandle, conn: &Connection, music_dir: &Path) -> Result<()> {
    let xml = export_xml::generate_xml(conn, music_dir)?;
    let target = itunes_library_path(app)?;
    backup_if_foreign(&target)?;
    std::fs::write(&target, xml)?;
    Ok(())
}

/// Fire-and-forget: only writes if the persisted toggle is on.
/// Must never interrupt the user's real action if the write fails.
pub fn write_if_enabled(app: &AppHandle, conn: &Connection, music_dir: &Path) {
    match db::get_auto_sync_xml(conn) {
        Ok(true) => {
            if let Err(e) = write_now(app, conn, music_dir) {
                eprintln!("[xml_sync] auto-sync failed: {e}");
            }
        }
        Ok(false) => {}
        Err(e) => eprintln!("[xml_sync] could not read the auto-sync toggle: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sway-xmlsync-test-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn no_backup_when_target_missing() {
        let dir = tmp_dir("missing");
        let target = dir.join("iTunes Music Library.xml");
        backup_if_foreign(&target).unwrap();
        assert!(!dir.join("iTunes Music Library.original.xml").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_backup_when_marker_present() {
        let dir = tmp_dir("ours");
        let target = dir.join("iTunes Music Library.xml");
        std::fs::write(&target, "<plist><key>Sway Generator</key><true/></plist>").unwrap();
        backup_if_foreign(&target).unwrap();
        assert!(target.exists(), "our own file must not be moved");
        assert!(!dir.join("iTunes Music Library.original.xml").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_renames_in_place_when_foreign() {
        let dir = tmp_dir("foreign");
        let target = dir.join("iTunes Music Library.xml");
        std::fs::write(&target, "<plist><key>Application Version</key><string>12.13.10.3</string></plist>").unwrap();
        backup_if_foreign(&target).unwrap();
        assert!(!target.exists(), "the original was renamed, it must not still be at the original path");
        let renamed = dir.join("iTunes Music Library.original.xml");
        assert!(renamed.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn second_foreign_backup_gets_timestamped_name() {
        let dir = tmp_dir("foreign-twice");
        let target = dir.join("iTunes Music Library.xml");
        std::fs::write(&target, "foreign v1").unwrap();
        backup_if_foreign(&target).unwrap();
        assert!(dir.join("iTunes Music Library.original.xml").exists());
        // Sway writes its own version (with marker), then real iTunes
        // overwrites it again with another foreign version -> the .original
        // already exists, the first one must not be lost: the second gets a
        // timestamp.
        std::fs::write(&target, "<key>Sway Generator</key>").unwrap();
        std::fs::write(&target, "foreign v2 (real iTunes rewrote it)").unwrap();
        backup_if_foreign(&target).unwrap();
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).collect();
        // .original.xml (v1) + one .bak-<timestamp>.xml (v2) = 2 backups.
        let bak_count = entries
            .iter()
            .filter(|e| e.file_name().to_string_lossy().contains(".bak-"))
            .count();
        assert_eq!(bak_count, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
