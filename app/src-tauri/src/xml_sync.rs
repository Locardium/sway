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

/// Si `target` existe y NO tiene nuestro marcador, lo copia a
/// `Sway-backups/` antes de que lo pisemos. Si tiene el marcador (lo
/// escribimos nosotros la ultima vez) no hace nada — backupear algo propio
/// no tiene sentido.
fn backup_if_foreign(target: &Path) -> Result<()> {
    if !target.exists() {
        return Ok(());
    }
    let contents = std::fs::read_to_string(target).unwrap_or_default();
    if contents.contains(MARKER) {
        return Ok(());
    }
    let backups_dir = target
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("Sway-backups");
    std::fs::create_dir_all(&backups_dir)?;
    let stem = target
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("iTunes Music Library");
    let ts = export_xml::compact_timestamp(SystemTime::now());
    let backup_path = backups_dir.join(format!("{stem}.bak-{ts}.xml"));
    std::fs::copy(target, &backup_path)?;
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
        let backups = dir.join("Sway-backups");
        assert!(!backups.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_backup_when_marker_present() {
        let dir = tmp_dir("ours");
        let target = dir.join("iTunes Music Library.xml");
        std::fs::write(&target, "<plist><key>Sway Generator</key><true/></plist>").unwrap();
        backup_if_foreign(&target).unwrap();
        let backups = dir.join("Sway-backups");
        assert!(!backups.exists() || std::fs::read_dir(&backups).unwrap().next().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_created_when_foreign() {
        let dir = tmp_dir("foreign");
        let target = dir.join("iTunes Music Library.xml");
        std::fs::write(&target, "<plist><key>Application Version</key><string>12.13.10.3</string></plist>").unwrap();
        backup_if_foreign(&target).unwrap();
        let backups = dir.join("Sway-backups");
        assert!(backups.exists());
        assert!(std::fs::read_dir(&backups).unwrap().next().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
