use anyhow::Result;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::prelude::{Accessor, ItemKey};
use rusqlite::{params, Connection};
use std::path::Path;
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

/// Inserta un archivo de audio (o lo encuentra si ya estaba) y devuelve su id.
fn import_one(conn: &Connection, path: &Path) -> Result<i64> {
    let m = read_meta(path);
    conn.execute(
        "INSERT OR IGNORE INTO tracks (path, title, artist, album, genre, duration_ms, bpm)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            path.to_string_lossy(),
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
        [path.to_string_lossy()],
        |r| r.get(0),
    )?;
    Ok(id)
}

/// Escanea `folder` recursivo, lee tags e inserta tracks nuevos. Devuelve
/// cuantos se insertaron (los ya presentes por `path` se ignoran).
pub fn import_folder(conn: &Connection, folder: &str) -> Result<usize> {
    let before: i64 = conn.query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))?;
    for entry in WalkDir::new(folder).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() && is_audio(entry.path()) {
            import_one(conn, entry.path())?;
        }
    }
    let after: i64 = conn.query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))?;
    Ok((after - before) as usize)
}

/// Importa una mezcla de archivos y carpetas (drop desde el OS). Devuelve los
/// ids de todos los tracks de audio involucrados (nuevos o ya existentes).
pub fn import_paths(conn: &Connection, paths: &[String]) -> Result<Vec<i64>> {
    let mut ids = Vec::new();
    for p in paths {
        let path = Path::new(p);
        if path.is_dir() {
            for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
                if entry.file_type().is_file() && is_audio(entry.path()) {
                    ids.push(import_one(conn, entry.path())?);
                }
            }
        } else if path.is_file() && is_audio(path) {
            ids.push(import_one(conn, path)?);
        }
    }
    Ok(ids)
}
