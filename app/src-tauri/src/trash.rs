//! Papelera propia de la biblioteca.
//!
//! Un borrado que llega por sync no destruye bytes. El archivo se mueve a
//! `<música>/.sway-trash/` y se queda ahí un tiempo largo antes de
//! desaparecer de verdad.
//!
//! El motivo es el requisito duro de toda la Fase 5: nunca perder música. Un
//! borrado local lo hiciste vos mirando la pantalla; uno que llega por la red
//! puede venir de un dispositivo desincronizado, de un tombstone viejo o de un
//! bug. La papelera es lo que hace que ese caso sea recuperable en vez de
//! definitivo.

use std::path::{Path, PathBuf};

/// Cuánto sobrevive un archivo borrado antes de que se limpie de verdad.
pub const RETENTION_DAYS: u64 = 30;

pub fn trash_dir(music_dir: &Path) -> PathBuf {
    music_dir.join(".sway-trash")
}

/// Manda un archivo a la papelera. Devuelve dónde quedó.
///
/// Nunca pisa: si ya hay algo con ese nombre (borraste dos archivos que se
/// llamaban igual), desambigua.
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
    // `rename` es lo barato y lo atómico; el fallback cubre el caso de que la
    // papelera termine en otro volumen que el archivo.
    match std::fs::rename(path, &dest) {
        Ok(()) => Ok(dest),
        Err(_) => {
            std::fs::copy(path, &dest)?;
            std::fs::remove_file(path)?;
            Ok(dest)
        }
    }
}

/// Borra de verdad lo que ya cumplió la retención. Se corre al arrancar.
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
        // Sin fecha legible se conserva: ante la duda, no borrar.
        let Ok(modified) = meta.modified() else { continue };
        if modified < cutoff && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        log::info!("[trash] {removed} archivo(s) purgados tras {retention_days} días");
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
        assert!(!f.exists(), "sale de la biblioteca");
        assert!(dest.exists(), "pero sigue existiendo");
        assert_eq!(std::fs::read(&dest).unwrap(), b"audio");
        std::fs::remove_dir_all(&music).ok();
    }

    /// Dos archivos distintos que se llamaban igual tienen que sobrevivir los
    /// dos: la papelera no puede pisar lo que ya rescató.
    #[test]
    fn a_name_collision_in_the_trash_does_not_overwrite() {
        let music = tmpdir("collide");
        for content in [b"primero".as_slice(), b"segundo".as_slice()] {
            let f = music.join("mismo.flac");
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
        let f = music.join("nuevo.flac");
        std::fs::write(&f, b"x").unwrap();
        move_to_trash(&music, &f).unwrap();

        // Recién borrado: la retención lo protege.
        assert_eq!(purge_old(&music, RETENTION_DAYS), 0);
        // Con retención cero, se va.
        assert_eq!(purge_old(&music, 0), 1);
        std::fs::remove_dir_all(&music).ok();
    }
}
