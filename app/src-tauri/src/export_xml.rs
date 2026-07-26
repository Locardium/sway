//! Generador puro del iTunes Music Library.xml (formato validado en Fase 0,
//! `tools/itunes-xml-poc/generate.mjs`). Sin conocimiento de rutas de SO ni
//! de Tauri: recibe la conexion a la DB y la carpeta gestionada, devuelve un
//! String. La escritura a disco + backup vive en `xml_sync`.

use crate::db::{self, PlaylistNode, Track};
use anyhow::Result;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const KIND_BY_EXT: &[(&str, &str)] = &[
    ("flac", "FLAC audio file"),
    ("mp3", "MPEG audio file"),
    ("wav", "WAV audio file"),
    ("m4a", "AAC audio file"),
    ("aac", "AAC audio file"),
    ("aif", "AIFF audio file"),
    ("aiff", "AIFF audio file"),
    ("ogg", "Ogg audio file"),
    ("opus", "Opus audio file"),
];

pub fn generate_xml(conn: &Connection, music_dir: &Path) -> Result<String> {
    let tracks = db::list_tracks(conn)?;
    let playlists = db::list_playlists(conn)?;

    let mut children: HashMap<Option<i64>, Vec<&PlaylistNode>> = HashMap::new();
    for p in &playlists {
        children.entry(p.parent_id).or_default().push(p);
    }

    let mut ctx = Ctx {
        conn,
        children,
        pid: 1001,
        desc_cache: HashMap::new(),
        blocks: String::new(),
    };

    let roots = ctx.children.get(&None).cloned().unwrap_or_default();
    for root in roots {
        ctx.emit_node(root, None)?;
    }

    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<!DOCTYPE plist PUBLIC \"-//Apple Computer//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n");
    xml.push_str("<plist version=\"1.0\">\n<dict>\n");
    xml.push_str("\t<key>Major Version</key><integer>1</integer>\n");
    xml.push_str("\t<key>Minor Version</key><integer>1</integer>\n");
    xml.push_str("\t<key>Application Version</key><string>12.13.10.3</string>\n");
    xml.push_str(&format!(
        "\t<key>Date</key><date>{}</date>\n",
        itunes_date(SystemTime::now())
    ));
    xml.push_str("\t<key>Features</key><integer>5</integer>\n");
    xml.push_str("\t<key>Show Content Ratings</key><true/>\n");
    xml.push_str(&format!(
        "\t<key>Library Persistent ID</key><string>{}</string>\n",
        fnv1a_hex(&music_dir.to_string_lossy())
    ));
    // Marca que este archivo lo escribio Sway (no iTunes real). xml_sync la
    // usa para decidir si hay que backupear antes de pisar el archivo.
    xml.push_str("\t<key>Sway Generator</key><true/>\n");

    xml.push_str("\t<key>Tracks</key>\n\t<dict>\n");
    for t in &tracks {
        xml.push_str(&track_dict(t));
    }
    xml.push_str("\t</dict>\n");

    xml.push_str("\t<key>Playlists</key>\n\t<array>\n");
    xml.push_str(&master_playlist(&tracks));
    xml.push_str(&ctx.blocks);
    xml.push_str("\t</array>\n");

    // iTunes pone Music Folder AL FINAL, despues de Playlists (Rekordbox lo exige asi).
    let music_folder = to_itunes_location(&ensure_trailing_sep(&music_dir.to_string_lossy()));
    xml.push_str(&format!(
        "\t<key>Music Folder</key><string>{}</string>\n",
        xml_escape(&music_folder)
    ));
    xml.push_str("</dict>\n</plist>\n");

    Ok(xml)
}

struct Ctx<'a> {
    conn: &'a Connection,
    children: HashMap<Option<i64>, Vec<&'a PlaylistNode>>,
    pid: i64,
    desc_cache: HashMap<i64, Vec<i64>>,
    blocks: String,
}

impl<'a> Ctx<'a> {
    fn emit_node(&mut self, node: &'a PlaylistNode, parent_persist: Option<String>) -> Result<()> {
        let my_pid = self.pid;
        self.pid += 1;
        let persist = persistent_id_playlist(node.id);

        if node.kind == "folder" {
            let track_ids = self.descendant_track_ids(node.id)?;
            self.blocks.push_str(&folder_entry(
                &node.name,
                my_pid,
                &persist,
                parent_persist.as_deref(),
                &track_ids,
            ));
            let kids = self.children.get(&Some(node.id)).cloned().unwrap_or_default();
            for kid in kids {
                self.emit_node(kid, Some(persist.clone()))?;
            }
        } else {
            let track_ids: Vec<i64> = db::playlist_tracks(self.conn, node.id)?
                .iter()
                .map(|t| t.id)
                .collect();
            self.blocks.push_str(&playlist_entry(
                &node.name,
                my_pid,
                &persist,
                parent_persist.as_deref(),
                &track_ids,
            ));
        }
        Ok(())
    }

    /// Union de los track ids de TODO el subarbol de `id` (memoizado). Es el
    /// fix que hace que Serato muestre el folder (ver dj-itunes-xml-compat).
    fn descendant_track_ids(&mut self, id: i64) -> Result<Vec<i64>> {
        if let Some(v) = self.desc_cache.get(&id) {
            return Ok(v.clone());
        }
        // Tracks asignados directamente a este nodo (defensivo: la UI hoy no
        // agrega tracks directo a un folder, pero el schema lo permite).
        let mut ids: Vec<i64> = db::playlist_tracks(self.conn, id)?
            .iter()
            .map(|t| t.id)
            .collect();
        let kids = self.children.get(&Some(id)).cloned().unwrap_or_default();
        for kid in kids {
            let sub = if kid.kind == "folder" {
                self.descendant_track_ids(kid.id)?
            } else {
                db::playlist_tracks(self.conn, kid.id)?
                    .iter()
                    .map(|t| t.id)
                    .collect()
            };
            ids.extend(sub);
        }
        let mut seen = HashSet::new();
        let deduped: Vec<i64> = ids.into_iter().filter(|i| seen.insert(*i)).collect();
        self.desc_cache.insert(id, deduped.clone());
        Ok(deduped)
    }
}

fn kv_string(key: &str, val: &str) -> String {
    format!(
        "\t\t\t<key>{}</key><string>{}</string>\n",
        xml_escape(key),
        xml_escape(val)
    )
}

fn kv_integer(key: &str, val: i64) -> String {
    format!("\t\t\t<key>{}</key><integer>{}</integer>\n", xml_escape(key), val)
}

fn kv_date(key: &str, val: &str) -> String {
    format!("\t\t\t<key>{}</key><date>{}</date>\n", xml_escape(key), val)
}

fn playlist_items_block(ids: &[i64]) -> String {
    let mut s = String::from("\t\t\t<key>Playlist Items</key>\n\t\t\t<array>\n");
    for id in ids {
        s.push_str(&format!(
            "\t\t\t\t<dict>\n\t\t\t\t\t<key>Track ID</key><integer>{id}</integer>\n\t\t\t\t</dict>\n"
        ));
    }
    s.push_str("\t\t\t</array>\n");
    s
}

fn folder_entry(name: &str, pid: i64, persist: &str, parent_persist: Option<&str>, track_ids: &[i64]) -> String {
    let mut s = String::from("\t\t<dict>\n");
    s.push_str(&kv_integer("Playlist ID", pid));
    if let Some(pp) = parent_persist {
        s.push_str(&kv_string("Parent Persistent ID", pp));
    }
    s.push_str(&kv_string("Playlist Persistent ID", persist));
    s.push_str("\t\t\t<key>All Items</key><true/>\n");
    s.push_str("\t\t\t<key>Folder</key><true/>\n");
    s.push_str(&kv_string("Name", name));
    s.push_str(&playlist_items_block(track_ids));
    s.push_str("\t\t</dict>\n");
    s
}

fn playlist_entry(name: &str, pid: i64, persist: &str, parent_persist: Option<&str>, track_ids: &[i64]) -> String {
    let mut s = String::from("\t\t<dict>\n");
    s.push_str(&kv_integer("Playlist ID", pid));
    if let Some(pp) = parent_persist {
        s.push_str(&kv_string("Parent Persistent ID", pp));
    }
    s.push_str(&kv_string("Playlist Persistent ID", persist));
    s.push_str("\t\t\t<key>All Items</key><true/>\n");
    s.push_str(&kv_string("Name", name));
    s.push_str(&playlist_items_block(track_ids));
    s.push_str("\t\t</dict>\n");
    s
}

fn master_playlist(tracks: &[Track]) -> String {
    let mut s = String::from("\t\t<dict>\n");
    s.push_str("\t\t\t<key>Master</key><true/>\n");
    s.push_str(&kv_integer("Playlist ID", 1000));
    // id=0 esta reservado para la maestra: los ids reales de playlists
    // arrancan en 1 (autoincrement de SQLite), asi que nunca choca.
    s.push_str(&kv_string("Playlist Persistent ID", &persistent_id_playlist(0)));
    s.push_str("\t\t\t<key>All Items</key><true/>\n");
    s.push_str("\t\t\t<key>Visible</key><false/>\n");
    s.push_str(&kv_string("Name", "Library"));
    let ids: Vec<i64> = tracks.iter().map(|t| t.id).collect();
    s.push_str(&playlist_items_block(&ids));
    s.push_str("\t\t</dict>\n");
    s
}

fn track_dict(t: &Track) -> String {
    let path = Path::new(&t.path);
    let stat = std::fs::metadata(path).ok();
    let mtime = stat
        .as_ref()
        .and_then(|m| m.modified().ok())
        .unwrap_or_else(SystemTime::now);
    let size = stat.as_ref().map(|m| m.len());

    let mut s = format!("\t\t<key>{}</key>\n\t\t<dict>\n", t.id);
    s.push_str(&kv_integer("Track ID", t.id));
    if let Some(sz) = size {
        s.push_str(&kv_integer("Size", sz as i64));
    }
    if t.duration_ms > 0 {
        s.push_str(&kv_integer("Total Time", t.duration_ms));
    }
    if let Some(bpm) = t.bpm {
        if bpm > 0 {
            s.push_str(&kv_integer("BPM", bpm));
        }
    }
    let date_str = itunes_date(mtime);
    s.push_str(&kv_date("Date Modified", &date_str));
    s.push_str(&kv_date("Date Added", &date_str));
    s.push_str(&kv_string("Persistent ID", &persistent_id_track(t.id)));
    s.push_str(&kv_string("Track Type", "File"));
    s.push_str(&kv_string("Name", &t.title));
    if !t.artist.is_empty() {
        s.push_str(&kv_string("Artist", &t.artist));
    }
    if !t.album.is_empty() {
        s.push_str(&kv_string("Album", &t.album));
    }
    if !t.genre.is_empty() {
        s.push_str(&kv_string("Genre", &t.genre));
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let kind = KIND_BY_EXT
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, k)| *k)
        .unwrap_or("Audio file");
    s.push_str(&kv_string("Kind", kind));
    s.push_str(&kv_string("Location", &to_itunes_location(&t.path)));
    s.push_str("\t\t</dict>\n");
    s
}

// ---------------------------------------------------------------------------
// Helpers de formato (sin dependencias nuevas)
// ---------------------------------------------------------------------------

/// XML 1.0 prohibe NUL y la mayoria de control chars. ID3 usa NUL para
/// separar valores multiples y quedan crudos en los tags; Serato RECHAZA el
/// XML entero si aparece un NUL. Colapsa runs de control chars (menos
/// tab/LF/CR) a " / ", despues escapa & < > ".
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if is_illegal_ctrl(c) {
            out.push_str(" / ");
            while let Some(&nc) = chars.peek() {
                if is_illegal_ctrl(nc) {
                    chars.next();
                } else {
                    break;
                }
            }
        } else {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                _ => out.push(c),
            }
        }
    }
    out
}

fn is_illegal_ctrl(c: char) -> bool {
    let u = c as u32;
    (0x00..=0x08).contains(&u) || u == 0x0B || u == 0x0C || (0x0E..=0x1F).contains(&u)
}

/// Descompone un SystemTime en componentes UTC (y, m, d, hh, mm, ss).
fn utc_parts(t: SystemTime) -> (i64, u32, u32, i64, i64, i64) {
    let dur = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs() as i64;
    let days = secs.div_euclid(86400);
    let secs_of_day = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    (y, m, d, secs_of_day / 3600, (secs_of_day % 3600) / 60, secs_of_day % 60)
}

/// Fecha formato iTunes: 2026-07-23T03:11:37Z (UTC, sin milisegundos).
fn itunes_date(t: SystemTime) -> String {
    let (y, m, d, hh, mm, ss) = utc_parts(t);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Timestamp compacto UTC (para nombres de archivo de backup en xml_sync).
pub(crate) fn compact_timestamp(t: SystemTime) -> String {
    let (y, m, d, hh, mm, ss) = utc_parts(t);
    format!("{y:04}{m:02}{d:02}-{hh:02}{mm:02}{ss:02}")
}

/// Howard Hinnant's civil_from_days: dias desde 1970-01-01 (UTC) -> (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y0 = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y0 + 1 } else { y0 };
    (y, m, d)
}

fn ensure_trailing_sep(s: &str) -> String {
    if s.ends_with('\\') || s.ends_with('/') {
        s.to_string()
    } else {
        format!("{s}\\")
    }
}

/// `<Location>` formato iTunes Windows: file://localhost/C:/Carpeta/archivo.flac
/// con cada segmento percent-codificado. El ':' del drive se conserva.
fn to_itunes_location(path_str: &str) -> String {
    let normalized = path_str.replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').collect();
    let is_win = parts
        .first()
        .map(|p| p.len() == 2 && p.as_bytes()[1] == b':' && p.as_bytes()[0].is_ascii_alphabetic())
        .unwrap_or(false);
    let encoded: Vec<String> = parts
        .iter()
        .enumerate()
        .map(|(i, seg)| {
            if is_win && i == 0 {
                seg.to_string()
            } else {
                percent_encode_segment(seg)
            }
        })
        .collect();
    let joined = encoded.join("/");
    if is_win {
        format!("file://localhost/{joined}")
    } else if joined.starts_with('/') {
        format!("file://localhost{joined}")
    } else {
        format!("file://localhost/{joined}")
    }
}

/// Igual al unreserved set de encodeURIComponent en JS (lo que uso y valido
/// el PoC): A-Z a-z 0-9 - _ . ! ~ * ' ( )
fn percent_encode_segment(seg: &str) -> String {
    let mut out = String::new();
    for b in seg.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '!' | '~' | '*' | '\'' | '(' | ')') {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Persistent ID = 16 hex uppercase. A diferencia del PoC (que hasheaba
/// paths porque no tenia ids estables), acá usamos directo el id de SQLite,
/// que ya es estable entre corridas.
fn persistent_id_track(id: i64) -> String {
    format!("{:016X}", id as u64)
}

/// Igual pero con el bit mas alto seteado, para que el espacio de ids de
/// playlist nunca choque con el de tracks aunque compartan el mismo numero.
fn persistent_id_playlist(id: i64) -> String {
    format!("{:016X}", (1u64 << 62) | (id as u64))
}

/// FNV-1a 64-bit, solo para el Library Persistent ID (un unico valor
/// derivado de la carpeta gestionada, no necesita mas que ser estable).
fn fnv1a_hex(s: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016X}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use std::path::PathBuf;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        // Reusa el mismo SCHEMA que db::open aplica, via una insercion minima:
        // db.rs no expone SCHEMA como pub, asi que armamos las tablas a mano
        // (mismo DDL) para no depender de internals del modulo.
        conn.execute_batch(
            "
            CREATE TABLE tracks (
                id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE,
                title TEXT NOT NULL DEFAULT '', artist TEXT NOT NULL DEFAULT '',
                album TEXT NOT NULL DEFAULT '', genre TEXT NOT NULL DEFAULT '',
                duration_ms INTEGER NOT NULL DEFAULT 0, bpm INTEGER
            );
            CREATE TABLE playlists (
                id INTEGER PRIMARY KEY, name TEXT NOT NULL,
                kind TEXT NOT NULL DEFAULT 'playlist',
                parent_id INTEGER REFERENCES playlists(id) ON DELETE CASCADE,
                position INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE playlist_tracks (
                playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
                track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
                position INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (playlist_id, track_id)
            );
            ",
        )
        .unwrap();
        conn
    }

    fn add_track(conn: &Connection, path: &str, title: &str) -> i64 {
        conn.execute(
            "INSERT INTO tracks (path, title) VALUES (?1, ?2)",
            rusqlite::params![path, title],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn doctype_matches_apple_itunes_exact() {
        let conn = mem();
        let xml = generate_xml(&conn, &PathBuf::from(r"C:\Music\Sway")).unwrap();
        assert!(xml.contains("-//Apple Computer//DTD PLIST 1.0//EN"));
        assert!(!xml.contains("-//Apple//DTD"));
    }

    #[test]
    fn music_folder_comes_after_playlists_array() {
        let conn = mem();
        let xml = generate_xml(&conn, &PathBuf::from(r"C:\Music\Sway")).unwrap();
        let playlists_close = xml.find("</array>\n\t<key>Music Folder</key>");
        assert!(playlists_close.is_some(), "Music Folder debe venir justo despues del cierre de Playlists");
    }

    #[test]
    fn contains_sway_generator_marker() {
        let conn = mem();
        let xml = generate_xml(&conn, &PathBuf::from(r"C:\Music\Sway")).unwrap();
        assert!(xml.contains("<key>Sway Generator</key><true/>"));
    }

    #[test]
    fn folder_playlist_items_is_union_of_children() {
        let mut conn = mem();
        let t1 = add_track(&conn, r"C:\Music\a.flac", "A");
        let t2 = add_track(&conn, r"C:\Music\b.flac", "B");
        let folder = db::create_playlist(&conn, "Sets", "folder", None).unwrap();
        let p1 = db::create_playlist(&conn, "P1", "playlist", Some(folder)).unwrap();
        let p2 = db::create_playlist(&conn, "P2", "playlist", Some(folder)).unwrap();
        db::add_tracks_to_playlist(&mut conn, p1, &[t1]).unwrap();
        db::add_tracks_to_playlist(&mut conn, p2, &[t2]).unwrap();

        let xml = generate_xml(&conn, &PathBuf::from(r"C:\Music\Sway")).unwrap();
        let folder_persist = persistent_id_playlist(folder);
        let folder_block_start = xml.find(&folder_persist).unwrap();
        let folder_block = &xml[folder_block_start..];
        let items_end = folder_block.find("</array>").unwrap();
        let items_block = &folder_block[..items_end];
        assert!(items_block.contains(&format!("<integer>{t1}</integer>")));
        assert!(items_block.contains(&format!("<integer>{t2}</integer>")));
    }

    #[test]
    fn xml_escape_collapses_nul_and_escapes_amp() {
        let out = xml_escape("Foo\u{0}Bar & Baz");
        assert_eq!(out, "Foo / Bar &amp; Baz");
    }

    #[test]
    fn persistent_ids_are_stable_across_calls() {
        let conn = mem();
        add_track(&conn, r"C:\Music\a.flac", "A");
        let xml1 = generate_xml(&conn, &PathBuf::from(r"C:\Music\Sway")).unwrap();
        let xml2 = generate_xml(&conn, &PathBuf::from(r"C:\Music\Sway")).unwrap();
        assert_eq!(xml1, xml2);
    }
}
