use anyhow::Result;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::prelude::{Accessor, ItemKey};
use rusqlite::{params, Connection};
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
    if let Ok(tagged) = lofty::read_from_path(path) {
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
            if let Some(b) = tag.get_string(&ItemKey::IntegerBpm) {
                m.bpm = b.parse().ok();
            }
        }
    }
    m
}

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase())
        .is_some_and(|ext| AUDIO_EXTS.contains(&ext.as_str()))
}

/// Destino dentro de la carpeta gestionada. Reusa si ya hay un archivo con el
/// mismo nombre y tamaño; si no, desambigua con " (n)".
fn managed_dest(managed: &Path, src: &Path) -> PathBuf {
    let name = src.file_name().unwrap_or_default();
    let dest = managed.join(name);
    if dest.exists() {
        let ssize = std::fs::metadata(src).ok().map(|m| m.len());
        let dsize = std::fs::metadata(&dest).ok().map(|m| m.len());
        if ssize.is_some() && ssize == dsize {
            return dest; // mismo archivo, reusar
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

/// Copia el archivo a la carpeta gestionada (si hace falta), lee tags e
/// inserta. Devuelve el id. Los tracks quedan siempre bajo `managed`.
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
    let m = read_meta(&dest);
    conn.execute(
        "INSERT OR IGNORE INTO tracks (path, title, artist, album, genre, duration_ms, bpm)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            dest.to_string_lossy(),
            m.title,
            m.artist,
            m.album,
            m.genre,
            m.duration_ms,
            m.bpm
        ],
    )?;
    let id = conn.query_row(
        "SELECT id FROM tracks WHERE path = ?1",
        [dest.to_string_lossy()],
        |r| r.get(0),
    )?;
    Ok(id)
}

/// Junta todos los archivos de audio bajo `roots` (expande directorios).
fn collect_audio(roots: &[&Path]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for root in roots {
        if root.is_dir() {
            for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
                if entry.file_type().is_file() && is_audio(entry.path()) {
                    files.push(entry.path().to_path_buf());
                }
            }
        } else if root.is_file() && is_audio(root) {
            files.push(root.to_path_buf());
        }
    }
    files
}

/// Escanea `folder` recursivo, copia a la carpeta gestionada e inserta los
/// nuevos. `progress(done, total)` se llama por cada archivo procesado.
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

/// Importa una mezcla de archivos y carpetas (drop desde el OS). Devuelve los
/// ids de todos los tracks involucrados. `progress(done, total)` por archivo.
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
