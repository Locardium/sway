//! Hash de contenido (blake3) de los archivos de la biblioteca.
//!
//! El hash es la identidad de los BYTES: es lo que se pide en una
//! transferencia, lo que se verifica al terminarla y lo que permite darse
//! cuenta de que dos dispositivos ya tienen el mismo archivo aunque lo hayan
//! importado por separado con nombres distintos.
//!
//! Restricciones que dan forma a este modulo:
//! - Bibliotecas de 100+ GB. Nunca leer un archivo entero en memoria, y el
//!   backfill inicial no puede bloquear la UI.
//! - Rehashear en cada arranque seria inaceptable: `(size, mtime)` funciona de
//!   cache. Si ninguno cambio, el hash guardado sigue valiendo.

use rusqlite::Connection;
use std::io::Read;
use std::path::Path;

const CHUNK: usize = 256 * 1024;

/// blake3 del archivo, leido de a pedazos.
pub fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// `(tamaño, mtime en ms)` del archivo.
pub fn file_stamp(path: &Path) -> std::io::Result<(i64, i64)> {
    let md = std::fs::metadata(path)?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Ok((md.len() as i64, mtime))
}

/// Hashea un track y guarda hash + stamp. No-op si el stamp guardado coincide
/// con el del archivo (nada cambio desde la ultima vez).
///
/// El backfill de arranque (`spawn_hash_backfill` en lib.rs) no lo usa a
/// proposito: necesita calcular el hash FUERA del lock de la DB para no
/// congelar la UI. Este es el camino de un solo track, y es el que va a usar
/// la verificacion post-transferencia en 5.4.
#[allow(dead_code)]
pub fn hash_track(conn: &Connection, id: i64, path: &Path) -> anyhow::Result<Option<String>> {
    let (size, mtime) = match file_stamp(path) {
        Ok(v) => v,
        // Archivo faltante: no es un error fatal — puede ser un track legacy
        // fuera de la carpeta gestionada, o un blob todavia no transferido.
        Err(_) => return Ok(None),
    };
    let current: (Option<String>, Option<i64>, Option<i64>) = conn.query_row(
        "SELECT content_hash, size_bytes, mtime_ms FROM tracks WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if let (Some(h), Some(s), Some(m)) = current {
        if s == size && m == mtime {
            return Ok(Some(h));
        }
    }
    let hash = hash_file(path)?;
    conn.execute(
        "UPDATE tracks SET content_hash = ?1, size_bytes = ?2, mtime_ms = ?3 WHERE id = ?4",
        rusqlite::params![hash, size, mtime, id],
    )?;
    Ok(Some(hash))
}

/// Tracks que todavia no tienen hash valido (nunca se hasheo, o el archivo
/// cambio de tamaño/fecha desde la ultima vez).
pub fn pending(conn: &Connection) -> rusqlite::Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, path FROM tracks
         WHERE content_hash IS NULL OR size_bytes IS NULL OR mtime_ms IS NULL",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_content_dependent() {
        let dir = std::env::temp_dir().join(format!("sway-hash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.bin");
        let b = dir.join("b.bin");
        // Mas grande que un chunk, para ejercitar el loop de lectura.
        let data: Vec<u8> = (0..(CHUNK * 2 + 7)).map(|i| (i % 251) as u8).collect();
        std::fs::write(&a, &data).unwrap();
        std::fs::write(&b, &data).unwrap();
        assert_eq!(hash_file(&a).unwrap(), hash_file(&b).unwrap());
        assert_eq!(hash_file(&a).unwrap(), hash_file(&a).unwrap());

        let mut changed = data.clone();
        *changed.last_mut().unwrap() ^= 0xFF;
        std::fs::write(&b, &changed).unwrap();
        assert_ne!(hash_file(&a).unwrap(), hash_file(&b).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hash_track_caches_by_stamp() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE tracks (id INTEGER PRIMARY KEY, path TEXT, content_hash TEXT,
                size_bytes INTEGER, mtime_ms INTEGER);",
        )
        .unwrap();
        let dir = std::env::temp_dir().join(format!("sway-stamp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("t.bin");
        std::fs::write(&f, b"hola").unwrap();
        conn.execute("INSERT INTO tracks (id, path) VALUES (1, ?1)", [f.to_str().unwrap()])
            .unwrap();

        let first = hash_track(&conn, 1, &f).unwrap().unwrap();
        assert!(pending(&conn).unwrap().is_empty());

        // Mismo stamp -> devuelve el guardado sin releer.
        assert_eq!(hash_track(&conn, 1, &f).unwrap().unwrap(), first);

        // Contenido distinto -> stamp distinto -> rehashea.
        std::fs::write(&f, b"otra cosa mariposa").unwrap();
        let second = hash_track(&conn, 1, &f).unwrap().unwrap();
        assert_ne!(first, second);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE tracks (id INTEGER PRIMARY KEY, path TEXT, content_hash TEXT,
                size_bytes INTEGER, mtime_ms INTEGER);
             INSERT INTO tracks (id, path) VALUES (1, '/no/existe.flac');",
        )
        .unwrap();
        assert!(hash_track(&conn, 1, Path::new("/no/existe.flac")).unwrap().is_none());
    }
}
