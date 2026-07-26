//! Ubicacion de SO + backup + escritura del iTunes Music Library.xml. Todo lo
//! que sabe de rutas de Windows y de "de quien es este archivo" vive acá,
//! separado del generador puro en `export_xml`.

use crate::db;
use crate::export_xml;
use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tauri::{AppHandle, Manager};

/// Key que `export_xml::generate_xml` mete en todo archivo que escribe Sway.
/// Sirve para distinguir "esto lo escribimos nosotros" (no hace falta
/// backup) de "esto es ajeno" (library original del user, o el archivo
/// reescrito por el iTunes/Music real desde la ultima vez que escribimos).
const MARKER: &str = "<key>Sway Generator</key>";

/// Ubicacion estandar de la library de iTunes en Windows
/// (`<Musica>\iTunes\iTunes Music Library.xml`). Solo Windows por ahora:
/// Fase 0 valido el formato ahi; el Music.app moderno de Mac requiere que el
/// user habilite "Share Library XML" a mano y la ruta ahi no esta validada.
#[cfg(target_os = "windows")]
pub fn itunes_library_path(app: &AppHandle) -> Result<PathBuf> {
    let dir = app
        .path()
        .audio_dir()
        .context("no se pudo resolver la carpeta de Musica")?
        .join("iTunes");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("iTunes Music Library.xml"))
}

#[cfg(not(target_os = "windows"))]
pub fn itunes_library_path(_app: &AppHandle) -> Result<PathBuf> {
    anyhow::bail!("auto-sync a iTunes solo soportado en Windows por ahora")
}

/// Si `target` existe y NO tiene nuestro marcador, lo renombra en el MISMO
/// path (misma carpeta que el original) antes de que lo pisemos. Si tiene el
/// marcador (lo escribimos nosotros la ultima vez) no hace nada — backupear
/// algo propio no tiene sentido.
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
    // Primera vez: nombre estable "original". Si ya existe (iTunes real
    // volvio a escribir el archivo despues), un timestamp para no pisarlo.
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

/// Genera + backupea-si-hace-falta + escribe. Camino compartido por el boton
/// manual "Sync now" y el auto-sync.
pub fn write_now(app: &AppHandle, conn: &Connection, music_dir: &Path) -> Result<()> {
    let xml = export_xml::generate_xml(conn, music_dir)?;
    let target = itunes_library_path(app)?;
    backup_if_foreign(&target)?;
    std::fs::write(&target, xml)?;
    Ok(())
}

/// Fire-and-forget: solo escribe si el toggle persistido esta prendido.
/// Nunca debe interrumpir la accion real del user si la escritura falla.
pub fn write_if_enabled(app: &AppHandle, conn: &Connection, music_dir: &Path) {
    match db::get_auto_sync_xml(conn) {
        Ok(true) => {
            if let Err(e) = write_now(app, conn, music_dir) {
                eprintln!("[xml_sync] auto-sync fallo: {e}");
            }
        }
        Ok(false) => {}
        Err(e) => eprintln!("[xml_sync] no se pudo leer el toggle de auto-sync: {e}"),
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
        assert!(target.exists(), "el archivo propio no se debe mover");
        assert!(!dir.join("iTunes Music Library.original.xml").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_renames_in_place_when_foreign() {
        let dir = tmp_dir("foreign");
        let target = dir.join("iTunes Music Library.xml");
        std::fs::write(&target, "<plist><key>Application Version</key><string>12.13.10.3</string></plist>").unwrap();
        backup_if_foreign(&target).unwrap();
        assert!(!target.exists(), "el original se renombro, no debe seguir en el path original");
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
        // Sway escribe una version propia (con marcador), despues iTunes real
        // la vuelve a pisar con otra version ajena -> el .original ya existe,
        // no hay que perder la primera: la segunda va con timestamp.
        std::fs::write(&target, "<key>Sway Generator</key>").unwrap();
        std::fs::write(&target, "foreign v2 (iTunes real reescribio)").unwrap();
        backup_if_foreign(&target).unwrap();
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).collect();
        // .original.xml (v1) + un .bak-<timestamp>.xml (v2) = 2 backups.
        let bak_count = entries
            .iter()
            .filter(|e| e.file_name().to_string_lossy().contains(".bak-"))
            .count();
        assert_eq!(bak_count, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
