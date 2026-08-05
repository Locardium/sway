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
            log::warn!("lofty read_from_path (con tags) fallo para {}: {e}", path.display());
            apply_meta_from_broken_tag(path, &mut m);
        }
    }
    m
}

/// Se llama cuando la pasada normal de lofty fallo. Un frame de tag
/// individualmente corrupto (ej. `TORY`/`TYER` con una fecha invalida —
/// visto en la practica con el texto de un sitio de descargas pisando el
/// campo) hace que lofty descarte el tag ENTERO, aunque el resto sea
/// perfectamente legible. Primero se intenta sanitizar esos frames y
/// reparsear en memoria (rescata todo: titulo/artista/caratula/duracion);
/// si eso no aplica o tampoco alcanza, una ultima pasada sin parsear tags
/// rescata al menos la duracion real (critica para el seek bar) en vez de
/// dejarla en 0. El titulo ya tiene fallback al nombre de archivo.
fn apply_meta_from_broken_tag(path: &Path, m: &mut Meta) {
    if let Ok(bytes) = std::fs::read(path) {
        if let Some(patched) = sanitize_id3v2_date_frames(&bytes) {
            let parsed = Probe::new(Cursor::new(patched.as_slice()))
                .guess_file_type()
                .ok()
                .and_then(|p| p.read().ok());
            if let Some(tagged) = parsed {
                log::info!("id3_sanitize: {} recupero el tag completo tras sanitizar", path.display());
                fill_meta_from_tagged(m, &tagged);
                return;
            }
            log::warn!("id3_sanitize: {} sigue sin parsear incluso sanitizado", path.display());
        }
    }
    let props_only = ParseOptions::new().read_tags(false);
    match Probe::open(path).and_then(|p| p.options(props_only).read()) {
        Ok(tagged) => {
            m.duration_ms = tagged.properties().duration().as_millis() as i64;
        }
        Err(e2) => {
            log::warn!("lofty properties-only tambien fallo para {}: {e2}", path.display());
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

/// Inserta el track ya ubicado en `dest` dentro de la carpeta gestionada, o
/// refresca sus tags si la fila ya existe (el path es UNIQUE). Compartido por
/// `import_one` (src en disco) e `import_bytes` (src en memoria, ver mas
/// abajo).
///
/// El refresh importa: reimportar un archivo que ya estaba en la biblioteca es
/// la unica forma que tiene el user de recuperar tags que un import viejo no
/// supo leer (ej. los tracks importados antes del fix de `id3_sanitize`). Con
/// `INSERT OR IGNORE` el reimport era un no-op silencioso y la fila quedaba
/// con los campos vacios para siempre. Cada campo solo se pisa si la lectura
/// nueva trae algo — una lectura peor (tag ilegible) nunca borra datos buenos.
fn insert_track(conn: &Connection, dest: &Path) -> Result<i64> {
    let m = read_meta(dest);
    conn.execute(
        "INSERT INTO tracks (path, title, artist, album, genre, duration_ms, bpm)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(path) DO UPDATE SET
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
            m.bpm
        ],
    )?;
    conn.query_row(
        "SELECT id FROM tracks WHERE path = ?1",
        [dest.to_string_lossy()],
        |r| r.get(0),
    )
    .map_err(Into::into)
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
    insert_track(conn, &dest)
}

/// Igual que `managed_dest` pero sin un archivo fuente en disco para
/// stat-ear (los bytes ya estan en memoria — ver `import_bytes`).
fn managed_dest_for(managed: &Path, name: &str, size: u64) -> PathBuf {
    let dest = managed.join(name);
    if dest.exists() {
        let dsize = std::fs::metadata(&dest).ok().map(|m| m.len());
        if dsize == Some(size) {
            return dest; // mismo archivo, reusar
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

/// Importa bytes ya leidos en memoria (Android: el picker da URIs
/// `content://` que Rust no abre con `std::fs`; el comando `import_from_uri`
/// en lib.rs las resuelve via `tauri_plugin_fs` y manda bytes + nombre
/// original acá). No valida contra `AUDIO_EXTS`: el picker ya filtro por
/// MIME type del lado del SO, y lofty detecta el formato real por contenido
/// aunque el nombre venga con una extension generica.
pub fn import_bytes(conn: &Connection, managed: &Path, name: &str, bytes: &[u8]) -> Result<i64> {
    let dest = managed_dest_for(managed, name, bytes.len() as u64);
    if !dest.exists() {
        std::fs::write(&dest, bytes)?;
    }
    insert_track(conn, &dest)
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
