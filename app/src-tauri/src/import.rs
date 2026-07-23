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

/// Escanea `folder` recursivo, lee tags e inserta tracks nuevos. Devuelve
/// cuantos se insertaron (los ya presentes por `path` se ignoran).
pub fn import_folder(conn: &Connection, folder: &str) -> Result<usize> {
    let mut inserted = 0usize;
    for entry in WalkDir::new(folder).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_lowercase())
            .unwrap_or_default();
        if !AUDIO_EXTS.contains(&ext.as_str()) {
            continue;
        }
        let m = read_meta(path);
        let n = conn.execute(
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
        inserted += n;
    }
    Ok(inserted)
}
