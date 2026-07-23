use rusqlite::{Connection, Result};
use serde::Serialize;
use std::path::Path;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub id: i64,
    pub path: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub duration_ms: i64,
    pub bpm: Option<i64>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tracks (
    id          INTEGER PRIMARY KEY,
    path        TEXT NOT NULL UNIQUE,
    title       TEXT NOT NULL DEFAULT '',
    artist      TEXT NOT NULL DEFAULT '',
    album       TEXT NOT NULL DEFAULT '',
    genre       TEXT NOT NULL DEFAULT '',
    duration_ms INTEGER NOT NULL DEFAULT 0,
    bpm         INTEGER
);

-- Jerarquia virtual (folders + playlists) — se usa desde Fase 2/3.
CREATE TABLE IF NOT EXISTS playlists (
    id        INTEGER PRIMARY KEY,
    name      TEXT NOT NULL,
    kind      TEXT NOT NULL DEFAULT 'playlist',  -- 'folder' | 'playlist'
    parent_id INTEGER REFERENCES playlists(id) ON DELETE CASCADE,
    position  INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS playlist_tracks (
    playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    track_id    INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    position    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (playlist_id, track_id)
);

CREATE TABLE IF NOT EXISTS cues (
    id       INTEGER PRIMARY KEY,
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    kind     TEXT NOT NULL,       -- 'hotcue' | 'memory' | 'loop'
    position_ms INTEGER NOT NULL,
    color    TEXT,
    label    TEXT
);
";

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    // Foldea el WAL de la sesion anterior al archivo principal en cada arranque.
    // Sin esto, un cierre sucio (force-kill en dev, cuelgue) deja todo el estado
    // en el -wal; si un arranque no lo aplica, la biblioteca "aparece vacia".
    // TRUNCATE consolida y achica el -wal a cero.
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    // Checkpoint automatico agresivo: no dejar crecer el -wal sin consolidar.
    conn.execute_batch("PRAGMA wal_autocheckpoint=256;")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

pub fn list_tracks(conn: &Connection) -> Result<Vec<Track>> {
    let mut stmt = conn.prepare(
        "SELECT id, path, title, artist, album, genre, duration_ms, bpm
         FROM tracks ORDER BY artist, album, title",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Track {
            id: r.get(0)?,
            path: r.get(1)?,
            title: r.get(2)?,
            artist: r.get(3)?,
            album: r.get(4)?,
            genre: r.get(5)?,
            duration_ms: r.get(6)?,
            bpm: r.get(7)?,
        })
    })?;
    rows.collect()
}

pub fn track_path(conn: &Connection, id: i64) -> Result<String> {
    conn.query_row("SELECT path FROM tracks WHERE id = ?1", [id], |r| r.get(0))
}

/// Borra tracks de la biblioteca. Los archivos que viven bajo la carpeta
/// gestionada `managed` se mandan a la papelera del OS; los de afuera (legacy)
/// no se tocan. El CASCADE los saca de todas las playlists.
pub fn delete_tracks(conn: &mut Connection, managed: &std::path::Path, ids: &[i64]) -> Result<()> {
    // Junta los paths antes de borrar de la DB.
    let mut paths = Vec::new();
    for id in ids {
        if let Ok(p) = track_path(conn, *id) {
            paths.push(p);
        }
    }
    let tx = conn.transaction()?;
    for id in ids {
        tx.execute("DELETE FROM tracks WHERE id = ?1", [id])?;
    }
    tx.commit()?;
    for p in paths {
        let path = std::path::Path::new(&p);
        if path.starts_with(managed) && path.exists() {
            let _ = trash::delete(path); // best-effort: no romper si falla
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Playlists / folders (jerarquia virtual)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistNode {
    pub id: i64,
    pub name: String,
    pub kind: String, // 'folder' | 'playlist'
    pub parent_id: Option<i64>,
    pub position: i64,
    pub track_count: i64,
}

pub fn list_playlists(conn: &Connection) -> Result<Vec<PlaylistNode>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.name, p.kind, p.parent_id, p.position,
                (SELECT COUNT(*) FROM playlist_tracks pt WHERE pt.playlist_id = p.id)
         FROM playlists p
         ORDER BY p.parent_id, p.position",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(PlaylistNode {
            id: r.get(0)?,
            name: r.get(1)?,
            kind: r.get(2)?,
            parent_id: r.get(3)?,
            position: r.get(4)?,
            track_count: r.get(5)?,
        })
    })?;
    rows.collect()
}

pub fn create_playlist(
    conn: &Connection,
    name: &str,
    kind: &str,
    parent_id: Option<i64>,
) -> Result<i64> {
    let pos: i64 = conn.query_row(
        "SELECT COALESCE(MAX(position) + 1, 0) FROM playlists WHERE parent_id IS ?1",
        [parent_id],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO playlists (name, kind, parent_id, position) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![name, kind, parent_id, pos],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn rename_playlist(conn: &Connection, id: i64, name: &str) -> Result<()> {
    conn.execute("UPDATE playlists SET name = ?1 WHERE id = ?2", rusqlite::params![name, id])?;
    Ok(())
}

pub fn delete_playlist(conn: &Connection, id: i64) -> Result<()> {
    // ON DELETE CASCADE limpia hijos y playlist_tracks.
    conn.execute("DELETE FROM playlists WHERE id = ?1", [id])?;
    Ok(())
}

/// True si `maybe_ancestor` es ancestro de (o igual a) `node`.
fn is_ancestor(conn: &Connection, maybe_ancestor: i64, node: i64) -> Result<bool> {
    let mut cur = Some(node);
    while let Some(id) = cur {
        if id == maybe_ancestor {
            return Ok(true);
        }
        cur = conn.query_row("SELECT parent_id FROM playlists WHERE id = ?1", [id], |r| r.get(0))?;
    }
    Ok(false)
}

/// Mueve un nodo a `new_parent` en el indice `index` entre sus hermanos.
pub fn move_playlist(
    conn: &mut Connection,
    id: i64,
    new_parent: Option<i64>,
    index: i64,
) -> std::result::Result<(), String> {
    if let Some(p) = new_parent {
        let kind: String = conn
            .query_row("SELECT kind FROM playlists WHERE id = ?1", [p], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if kind != "folder" {
            return Err("el destino no es una carpeta".into());
        }
        if is_ancestor(conn, id, p).map_err(|e| e.to_string())? {
            return Err("no se puede mover una carpeta dentro de sí misma".into());
        }
    }
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    {
        // Hermanos del destino, sin el nodo movido, en orden actual.
        let mut stmt = tx
            .prepare("SELECT id FROM playlists WHERE parent_id IS ?1 AND id != ?2 ORDER BY position")
            .map_err(|e| e.to_string())?;
        let mut siblings: Vec<i64> = stmt
            .query_map(rusqlite::params![new_parent, id], |r| r.get(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<_>>()
            .map_err(|e| e.to_string())?;
        let idx = (index.max(0) as usize).min(siblings.len());
        siblings.insert(idx, id);
        drop(stmt);
        tx.execute("UPDATE playlists SET parent_id = ?1 WHERE id = ?2", rusqlite::params![new_parent, id])
            .map_err(|e| e.to_string())?;
        for (i, sid) in siblings.iter().enumerate() {
            tx.execute("UPDATE playlists SET position = ?1 WHERE id = ?2", rusqlite::params![i as i64, sid])
                .map_err(|e| e.to_string())?;
        }
    }
    tx.commit().map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Tracks dentro de una playlist
// ---------------------------------------------------------------------------

pub fn playlist_tracks(conn: &Connection, playlist_id: i64) -> Result<Vec<Track>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.path, t.title, t.artist, t.album, t.genre, t.duration_ms, t.bpm
         FROM playlist_tracks pt JOIN tracks t ON t.id = pt.track_id
         WHERE pt.playlist_id = ?1
         ORDER BY pt.position",
    )?;
    let rows = stmt.query_map([playlist_id], |r| {
        Ok(Track {
            id: r.get(0)?,
            path: r.get(1)?,
            title: r.get(2)?,
            artist: r.get(3)?,
            album: r.get(4)?,
            genre: r.get(5)?,
            duration_ms: r.get(6)?,
            bpm: r.get(7)?,
        })
    })?;
    rows.collect()
}

/// Agrega tracks al final. Ignora los que ya estan. Devuelve cuantos agrego.
pub fn add_tracks_to_playlist(
    conn: &mut Connection,
    playlist_id: i64,
    track_ids: &[i64],
) -> Result<usize> {
    let tx = conn.transaction()?;
    let mut added = 0;
    for tid in track_ids {
        let pos: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM playlist_tracks WHERE playlist_id = ?1",
            [playlist_id],
            |r| r.get(0),
        )?;
        let n = tx.execute(
            "INSERT OR IGNORE INTO playlist_tracks (playlist_id, track_id, position) VALUES (?1, ?2, ?3)",
            rusqlite::params![playlist_id, tid, pos],
        )?;
        added += n;
    }
    tx.commit()?;
    Ok(added)
}

pub fn remove_tracks_from_playlist(
    conn: &mut Connection,
    playlist_id: i64,
    track_ids: &[i64],
) -> Result<()> {
    let tx = conn.transaction()?;
    for tid in track_ids {
        tx.execute(
            "DELETE FROM playlist_tracks WHERE playlist_id = ?1 AND track_id = ?2",
            rusqlite::params![playlist_id, tid],
        )?;
    }
    reindex_playlist(&tx, playlist_id)?;
    tx.commit()
}

/// Mueve un bloque de tracks (en su orden actual) al indice `index`.
pub fn reorder_playlist_tracks(
    conn: &mut Connection,
    playlist_id: i64,
    track_ids: &[i64],
    index: i64,
) -> Result<()> {
    let tx = conn.transaction()?;
    let mut order: Vec<i64> = {
        let mut stmt = tx.prepare(
            "SELECT track_id FROM playlist_tracks WHERE playlist_id = ?1 ORDER BY position",
        )?;
        let rows = stmt.query_map([playlist_id], |r| r.get(0))?;
        rows.collect::<Result<_>>()?
    };
    let moving: Vec<i64> = order.iter().copied().filter(|t| track_ids.contains(t)).collect();
    // Indice pedido es sobre la lista completa; ajusta por los que se sacan antes.
    let before = order
        .iter()
        .take((index.max(0) as usize).min(order.len()))
        .filter(|t| track_ids.contains(t))
        .count();
    order.retain(|t| !track_ids.contains(t));
    let idx = ((index.max(0) as usize).min(order.len() + moving.len()) - before).min(order.len());
    for (i, t) in moving.iter().enumerate() {
        order.insert(idx + i, *t);
    }
    for (i, t) in order.iter().enumerate() {
        tx.execute(
            "UPDATE playlist_tracks SET position = ?1 WHERE playlist_id = ?2 AND track_id = ?3",
            rusqlite::params![i as i64, playlist_id, t],
        )?;
    }
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn
    }

    fn add_track(conn: &Connection, path: &str) -> i64 {
        conn.execute("INSERT INTO tracks (path) VALUES (?1)", [path]).unwrap();
        conn.last_insert_rowid()
    }

    fn order_of(conn: &Connection, pid: i64) -> Vec<i64> {
        playlist_tracks(conn, pid).unwrap().iter().map(|t| t.id).collect()
    }

    #[test]
    fn create_assigns_sequential_positions() {
        let conn = mem();
        create_playlist(&conn, "a", "playlist", None).unwrap();
        create_playlist(&conn, "b", "playlist", None).unwrap();
        let nodes = list_playlists(&conn).unwrap();
        assert_eq!(nodes.iter().map(|n| n.position).collect::<Vec<_>>(), vec![0, 1]);
    }

    #[test]
    fn move_into_folder_and_reorder_siblings() {
        let mut conn = mem();
        let f = create_playlist(&conn, "f", "folder", None).unwrap();
        let a = create_playlist(&conn, "a", "playlist", None).unwrap();
        let b = create_playlist(&conn, "b", "playlist", None).unwrap();
        // a adentro de f
        move_playlist(&mut conn, a, Some(f), 0).unwrap();
        let nodes = list_playlists(&conn).unwrap();
        assert_eq!(nodes.iter().find(|n| n.id == a).unwrap().parent_id, Some(f));
        // b antes de f en la raiz
        move_playlist(&mut conn, b, None, 0).unwrap();
        let roots: Vec<i64> = list_playlists(&conn)
            .unwrap()
            .into_iter()
            .filter(|n| n.parent_id.is_none())
            .map(|n| n.id)
            .collect();
        assert_eq!(roots, vec![b, f]);
    }

    #[test]
    fn move_rejects_cycle_and_non_folder_target() {
        let mut conn = mem();
        let f1 = create_playlist(&conn, "f1", "folder", None).unwrap();
        let f2 = create_playlist(&conn, "f2", "folder", Some(f1)).unwrap();
        let p = create_playlist(&conn, "p", "playlist", None).unwrap();
        assert!(move_playlist(&mut conn, f1, Some(f2), 0).is_err()); // ciclo
        assert!(move_playlist(&mut conn, f1, Some(f1), 0).is_err()); // en si mismo
        assert!(move_playlist(&mut conn, f2, Some(p), 0).is_err()); // playlist no es carpeta
    }

    #[test]
    fn add_ignores_duplicates_and_appends() {
        let mut conn = mem();
        let pl = create_playlist(&conn, "p", "playlist", None).unwrap();
        let t1 = add_track(&conn, "/a");
        let t2 = add_track(&conn, "/b");
        assert_eq!(add_tracks_to_playlist(&mut conn, pl, &[t1, t2]).unwrap(), 2);
        assert_eq!(add_tracks_to_playlist(&mut conn, pl, &[t1]).unwrap(), 0);
        assert_eq!(order_of(&conn, pl), vec![t1, t2]);
    }

    #[test]
    fn reorder_moves_block_to_index() {
        let mut conn = mem();
        let pl = create_playlist(&conn, "p", "playlist", None).unwrap();
        let ids: Vec<i64> = (0..5).map(|i| add_track(&conn, &format!("/t{i}"))).collect();
        add_tracks_to_playlist(&mut conn, pl, &ids).unwrap();

        // Mover el primero al final (index = len).
        reorder_playlist_tracks(&mut conn, pl, &[ids[0]], 5).unwrap();
        assert_eq!(order_of(&conn, pl), vec![ids[1], ids[2], ids[3], ids[4], ids[0]]);

        // Mover bloque {ids[3], ids[4]} (posiciones 2,3 ahora) al inicio.
        reorder_playlist_tracks(&mut conn, pl, &[ids[3], ids[4]], 0).unwrap();
        assert_eq!(order_of(&conn, pl), vec![ids[3], ids[4], ids[1], ids[2], ids[0]]);

        // Mover al medio: ids[0] al index 2.
        reorder_playlist_tracks(&mut conn, pl, &[ids[0]], 2).unwrap();
        assert_eq!(order_of(&conn, pl), vec![ids[3], ids[4], ids[0], ids[1], ids[2]]);
    }

    #[test]
    fn remove_reindexes() {
        let mut conn = mem();
        let pl = create_playlist(&conn, "p", "playlist", None).unwrap();
        let ids: Vec<i64> = (0..3).map(|i| add_track(&conn, &format!("/r{i}"))).collect();
        add_tracks_to_playlist(&mut conn, pl, &ids).unwrap();
        remove_tracks_from_playlist(&mut conn, pl, &[ids[0]]).unwrap();
        assert_eq!(order_of(&conn, pl), vec![ids[1], ids[2]]);
        let positions: Vec<i64> = {
            let mut stmt = conn
                .prepare("SELECT position FROM playlist_tracks WHERE playlist_id = ?1 ORDER BY position")
                .unwrap();
            let rows = stmt.query_map([pl], |r| r.get(0)).unwrap();
            rows.collect::<Result<_>>().unwrap()
        };
        assert_eq!(positions, vec![0, 1]);
    }

    #[test]
    fn delete_folder_cascades() {
        let mut conn = mem();
        let f = create_playlist(&conn, "f", "folder", None).unwrap();
        let pl = create_playlist(&conn, "p", "playlist", Some(f)).unwrap();
        let t = add_track(&conn, "/x");
        add_tracks_to_playlist(&mut conn, pl, &[t]).unwrap();
        delete_playlist(&conn, f).unwrap();
        assert!(list_playlists(&conn).unwrap().is_empty());
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM playlist_tracks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
        // El track sigue en la biblioteca.
        assert_eq!(list_tracks(&conn).unwrap().len(), 1);
    }
}

fn reindex_playlist(tx: &rusqlite::Transaction, playlist_id: i64) -> Result<()> {
    let order: Vec<i64> = {
        let mut stmt = tx.prepare(
            "SELECT track_id FROM playlist_tracks WHERE playlist_id = ?1 ORDER BY position",
        )?;
        let rows = stmt.query_map([playlist_id], |r| r.get(0))?;
        rows.collect::<Result<_>>()?
    };
    for (i, t) in order.iter().enumerate() {
        tx.execute(
            "UPDATE playlist_tracks SET position = ?1 WHERE playlist_id = ?2 AND track_id = ?3",
            rusqlite::params![i as i64, playlist_id, t],
        )?;
    }
    Ok(())
}
