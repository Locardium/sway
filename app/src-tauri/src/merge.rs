//! Aplicación de cambios de metadata, playlists y carpetas (Fase 5.5).
//!
//! 5.4 movía archivos; esto mueve la organización: nombres, jerarquía de
//! carpetas, orden, y qué track está en qué playlist.
//!
//! Reglas, todas con el mismo sesgo — ante la duda, conservar:
//!
//! - **Metadata: gana el más nuevo** (`updated_at`). Sin empates: si son
//!   iguales no se toca nada, así que sincronizar dos veces seguidas no
//!   cambia nada la segunda vez.
//! - **Playlists: gana la más nueva**, pero sólo para nombre, padre y orden.
//!   Una playlist sólo desaparece con un tombstone explícito.
//! - **Membresías: unión.** Un track entra a una playlist si algún
//!   dispositivo lo puso ahí; sale sólo con un tombstone explícito. Agregar
//!   le gana a quitar concurrente, que es el sesgo correcto cuando lo que
//!   está en juego es perder música de un set.
//!
//! - **Borrados (5.6): se aplican según la política del dispositivo que los
//!   manda**, y el archivo va a la papelera de la biblioteca, no al vacío. El
//!   tombstone se guarda siempre, aunque no se borre nada: es lo que impide
//!   devolverle al otro lo que el otro ya sacó.

use crate::manifest::{Manifest, Membership, PlaylistEntry, ScopeEntry, DeviceSync, TrackEntry, Tombstone};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};


/// Conjunto de cambios listo para aplicar del otro lado.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Changes {
    pub tracks: Vec<TrackEntry>,
    pub playlists: Vec<PlaylistEntry>,
    pub memberships: Vec<Membership>,
    pub tombstones: Vec<Tombstone>,
    /// Scope selectivo (Fase 5.7). `default` para poder hablar con una versión
    /// anterior sin romper.
    #[serde(default)]
    pub scopes: Vec<ScopeEntry>,
    #[serde(default)]
    pub device_sync: Vec<DeviceSync>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Applied {
    pub tracks: usize,
    pub playlists: usize,
    pub memberships: usize,
    /// Cosas borradas por tombstones entrantes (Fase 5.6).
    pub deleted: usize,
    /// Filas de scope selectivo aplicadas (Fase 5.7).
    #[serde(default)]
    pub scope: usize,
    /// Borrados encolados esperando confirmación (política `ask`).
    #[serde(default)]
    pub queued: usize,
}

impl Applied {
    pub fn total(&self) -> usize {
        self.tracks + self.playlists + self.memberships + self.deleted + self.scope
    }
}

/// Qué de `local` le falta o le quedó viejo a `remote`. Es lo que hay que
/// mandarle para que quede al día.
/// Índices del manifest del otro lado. Se arman una vez y se consultan miles
/// de veces: con `find`/`any` sobre las listas, comparar dos bibliotecas de
/// veinte mil tracks son cientos de millones de comparaciones de strings — y
/// esto corre dos veces por sync, una por dirección.
struct Index<'a> {
    tracks: std::collections::HashMap<&'a str, &'a TrackEntry>,
    playlists: std::collections::HashMap<&'a str, &'a PlaylistEntry>,
    memberships: std::collections::HashMap<String, &'a Membership>,
    /// (entidad, uid) -> cuándo se borró.
    tombstones: std::collections::HashMap<(&'a str, &'a str), i64>,
    scopes: std::collections::HashMap<(&'a str, &'a str), &'a ScopeEntry>,
    device_sync: std::collections::HashMap<&'a str, &'a DeviceSync>,
}

impl<'a> Index<'a> {
    fn of(m: &'a Manifest) -> Self {
        Index {
            tracks: m.tracks.iter().map(|t| (t.uid.as_str(), t)).collect(),
            playlists: m.playlists.iter().map(|p| (p.uid.as_str(), p)).collect(),
            memberships: m
                .memberships
                .iter()
                .map(|x| (format!("{}:{}", x.playlist_uid, x.track_uid), x))
                .collect(),
            tombstones: m
                .tombstones
                .iter()
                .map(|t| ((t.entity.as_str(), t.uid.as_str()), t.deleted_at))
                .collect(),
            scopes: m
                .scopes
                .iter()
                .map(|e| ((e.device_uid.as_str(), e.playlist_uid.as_str()), e))
                .collect(),
            device_sync: m
                .device_sync
                .iter()
                .map(|d| (d.device_uid.as_str(), d))
                .collect(),
        }
    }
}

pub fn changes_for_peer(local: &Manifest, remote: &Manifest) -> Changes {
    let mut out = Changes::default();
    let they = Index::of(remote);

    for t in &local.tracks {
        let theirs = they.tracks.get(t.uid.as_str()).copied();
        let send = match theirs {
            // Sólo si lo nuestro es estrictamente más nuevo: con `>=` se
            // reenviaría todo en cada sync sin cambiar nada.
            Some(r) => t.updated_at > r.updated_at,
            // Si no lo tiene, la fila viaja con el archivo (5.4). Mandar
            // metadata de un track cuyo archivo no está crearía una entrada
            // fantasma en la biblioteca del otro.
            None => false,
        };
        if send {
            out.tracks.push(t.clone());
        }
    }

    for p in &local.playlists {
        if they.tombstones.contains_key(&("playlist", p.uid.as_str())) {
            continue;
        }
        match they.playlists.get(p.uid.as_str()) {
            Some(r) if p.updated_at > r.updated_at => out.playlists.push(p.clone()),
            None => out.playlists.push(p.clone()),
            _ => {}
        }
    }

    // Las membresias viajan por dos motivos distintos: porque al otro le
    // falta el par (union), o porque el ORDEN cambio. Sin lo segundo, un
    // reordenamiento no se propagaba nunca: el par ya existia de los dos
    // lados y nadie lo volvia a mandar.
    let mine_pl: std::collections::HashMap<&str, i64> = local
        .playlists
        .iter()
        .map(|p| (p.uid.as_str(), p.updated_at))
        .collect();
    for m in &local.memberships {
        let key = format!("{}:{}", m.playlist_uid, m.track_uid);
        // Un tombstone del otro lado sólo manda si es POSTERIOR al agregado.
        // Si no, volver a meter una canción en una playlist se deshacía solo:
        // el borrado viejo le ganaba al agregado nuevo, para siempre.
        if let Some(deleted_at) = they.tombstones.get(&("playlist_track", key.as_str())) {
            if *deleted_at >= m.added_at {
                continue;
            }
        }
        match they.memberships.get(&key) {
            None => out.memberships.push(m.clone()),
            // El orden es de quien toco la playlist mas recientemente. Las
            // membresias no tienen reloj propio; la playlist si.
            Some(r) if r.rank != m.rank => {
                let mine = mine_pl.get(m.playlist_uid.as_str()).copied().unwrap_or(0);
                let theirs = they
                    .playlists
                    .get(m.playlist_uid.as_str())
                    .map(|p| p.updated_at)
                    .unwrap_or(0);
                if mine > theirs {
                    out.memberships.push(m.clone());
                }
            }
            _ => {}
        }
    }

    out.tombstones = local.tombstones.clone();

    // Scope: se manda lo que del otro lado falta o quedó viejo. Va en los dos
    // sentidos como cualquier otro dato replicado — cada dispositivo puede
    // editar el scope de todos, incluido el propio.
    for e in &local.scopes {
        let theirs = they
            .scopes
            .get(&(e.device_uid.as_str(), e.playlist_uid.as_str()));
        if theirs.map(|r| e.updated_at > r.updated_at).unwrap_or(true) {
            out.scopes.push(e.clone());
        }
    }
    for m in &local.device_sync {
        let theirs = they.device_sync.get(m.device_uid.as_str());
        if theirs.map(|r| m.updated_at > r.updated_at).unwrap_or(true) {
            out.device_sync.push(m.clone());
        }
    }
    out
}

fn has_tombstone(conn: &Connection, entity: &str, uid: &str) -> bool {
    tombstone_at(conn, entity, uid).is_some()
}

/// `deleted_at` del tombstone, si lo hay.
fn tombstone_at(conn: &Connection, entity: &str, uid: &str) -> Option<i64> {
    conn.query_row(
        "SELECT deleted_at FROM tombstones WHERE entity = ?1 AND uid = ?2",
        rusqlite::params![entity, uid],
        |r| r.get(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// Aplica los cambios recibidos, incluidos los borrados según `policy`.
/// `peer_uid` es de quién vienen: queda anotado en la cola de confirmación.
pub fn apply(
    conn: &Connection,
    changes: &Changes,
    music_dir: &std::path::Path,
    policy: DeletePolicy,
    peer_uid: &str,
) -> rusqlite::Result<Applied> {
    let mut applied = Applied::default();

    // --- Metadata de tracks -------------------------------------------------
    for t in &changes.tracks {
        let local: Option<(i64, i64)> = conn
            .query_row(
                "SELECT id, updated_at FROM tracks WHERE uid = ?1",
                [&t.uid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        // Un track que no está acá no se crea desde metadata suelta: su fila
        // llega junto con el archivo (5.4). Si no, quedaría una entrada que
        // no se puede reproducir.
        let Some((id, local_updated)) = local else { continue };
        if t.updated_at <= local_updated {
            continue;
        }
        conn.execute(
            "UPDATE tracks SET title = ?1, artist = ?2, album = ?3, genre = ?4,
                    duration_ms = ?5, bpm = ?6, updated_at = ?7
             WHERE id = ?8",
            rusqlite::params![
                t.title,
                t.artist,
                t.album,
                t.genre,
                t.duration_ms,
                t.bpm,
                t.updated_at,
                id
            ],
        )?;
        applied.tracks += 1;
    }

    // --- Playlists y carpetas ----------------------------------------------
    //
    // Dos pasadas: primero existen todas (sin padre), después se enganchan.
    // Vienen en cualquier orden y una carpeta puede llegar después de sus
    // hijos; resolver el padre en una sola pasada dejaría nodos colgados.
    for p in &changes.playlists {
        if has_tombstone(conn, "playlist", &p.uid) {
            continue;
        }
        let existing: Option<(i64, i64)> = conn
            .query_row(
                "SELECT id, updated_at FROM playlists WHERE uid = ?1",
                [&p.uid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match existing {
            Some((id, local_updated)) if p.updated_at > local_updated => {
                conn.execute(
                    "UPDATE playlists SET name = ?1, kind = ?2, rank = ?3, updated_at = ?4
                     WHERE id = ?5",
                    rusqlite::params![p.name, p.kind, p.rank, p.updated_at, id],
                )?;
                applied.playlists += 1;
            }
            Some(_) => {}
            None => {
                conn.execute(
                    "INSERT INTO playlists (uid, name, kind, parent_id, rank, updated_at)
                     VALUES (?1, ?2, ?3, NULL, ?4, ?5)",
                    rusqlite::params![p.uid, p.name, p.kind, p.rank, p.updated_at],
                )?;
                applied.playlists += 1;
            }
        }
    }
    // Segunda pasada: ahora sí los padres existen.
    for p in &changes.playlists {
        if has_tombstone(conn, "playlist", &p.uid) {
            continue;
        }
        let parent_id: Option<i64> = match &p.parent_uid {
            Some(puid) => conn
                .query_row("SELECT id FROM playlists WHERE uid = ?1", [puid], |r| r.get(0))
                .optional()?,
            None => None,
        };
        conn.execute(
            "UPDATE playlists SET parent_id = ?1 WHERE uid = ?2",
            rusqlite::params![parent_id, p.uid],
        )?;
    }

    // --- Membresías ---------------------------------------------------------
    for m in &changes.memberships {
        let key = format!("{}:{}", m.playlist_uid, m.track_uid);
        // Igual que arriba: el tombstone local sólo gana si es posterior al
        // agregado que llega.
        if let Some(deleted_at) = tombstone_at(conn, "playlist_track", &key) {
            if deleted_at >= m.added_at {
                continue;
            }
            // El agregado es más nuevo: el borrado quedó viejo y estorba.
            conn.execute(
                "DELETE FROM tombstones WHERE entity = 'playlist_track' AND uid = ?1",
                [&key],
            )?;
        }
        let ids: Option<(i64, i64)> = conn
            .query_row(
                "SELECT p.id, t.id FROM playlists p, tracks t
                 WHERE p.uid = ?1 AND t.uid = ?2",
                rusqlite::params![m.playlist_uid, m.track_uid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        // Si el track todavía no llegó a este dispositivo, la membresía se
        // ignora: el próximo sync la trae, cuando el track exista.
        let Some((pid, tid)) = ids else { continue };
        // `DO UPDATE` y no `OR IGNORE`: un par que ya existe puede venir con
        // un rank distinto, que es como viaja un reordenamiento.
        let n = conn.execute(
            "INSERT INTO playlist_tracks (playlist_id, track_id, rank, added_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(playlist_id, track_id) DO UPDATE SET rank = excluded.rank
             WHERE playlist_tracks.rank <> excluded.rank",
            rusqlite::params![pid, tid, m.rank, m.added_at],
        )?;
        applied.memberships += n;
    }

    // --- Scope selectivo (Fase 5.7) ----------------------------------------
    //
    // Dato replicado como cualquier otro, con LWW por fila: el scope del
    // celular se edita desde la PC y al revés.
    for e in &changes.scopes {
        if crate::scope::apply_entry(conn, e)? {
            applied.scope += 1;
        }
    }
    for m in &changes.device_sync {
        if crate::scope::apply_device_sync(conn, m)? {
            applied.scope += 1;
        }
    }

    // --- Tombstones (Fase 5.6) ---------------------------------------------
    //
    // Se guardan SIEMPRE, aunque la política no borre: es lo que impide que
    // este dispositivo le devuelva al otro lo que el otro ya borró. Aplicarlos
    // o no es una decisión aparte.
    for t in &changes.tombstones {
        // Un borrado que el usuario ya rechazó no vuelve a entrar. Sin esto el
        // tombstone del otro lado reaparece en cada sync y la cola pregunta lo
        // mismo para siempre.
        if is_ignored(conn, &t.entity, &t.uid)? {
            continue;
        }
        conn.execute(
            "INSERT OR IGNORE INTO tombstones (entity, uid, deleted_at, device_uid)
             VALUES (?1, ?2, ?3, '')",
            rusqlite::params![t.entity, t.uid, t.deleted_at],
        )?;
        match policy {
            DeletePolicy::Propagate => applied.deleted += apply_tombstone(conn, music_dir, t)?,
            DeletePolicy::Ask => applied.queued += enqueue_delete(conn, t, peer_uid)?,
            DeletePolicy::Ignore => {}
        }
    }

    Ok(applied)
}

/// True si el usuario ya rechazó ese borrado.
fn is_ignored(conn: &Connection, entity: &str, uid: &str) -> rusqlite::Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM delete_ignores WHERE entity = ?1 AND uid = ?2",
        rusqlite::params![entity, uid],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Encola un borrado para que el usuario lo confirme (política `ask`).
/// Devuelve 1 si quedó encolado, 0 si no había nada que borrar acá.
fn enqueue_delete(
    conn: &Connection,
    t: &Tombstone,
    peer_uid: &str,
) -> rusqlite::Result<usize> {
    let label: Option<String> = match t.entity.as_str() {
        "track" => conn
            .query_row(
                "SELECT CASE WHEN title <> '' THEN title ELSE rel_path END
                 FROM tracks WHERE uid = ?1",
                [&t.uid],
                |r| r.get(0),
            )
            .optional()?,
        "playlist" => conn
            .query_row("SELECT name FROM playlists WHERE uid = ?1", [&t.uid], |r| r.get(0))
            .optional()?,
        "playlist_track" => {
            let Some((pl, tr)) = t.uid.split_once(':') else { return Ok(0) };
            conn.query_row(
                "SELECT t.title || ' — ' || p.name FROM playlists p, tracks t
                 WHERE p.uid = ?1 AND t.uid = ?2",
                rusqlite::params![pl, tr],
                |r| r.get(0),
            )
            .optional()?
        }
        _ => None,
    };
    // Nada que borrar acá: no hay nada que preguntar.
    let Some(label) = label else { return Ok(0) };
    let n = conn.execute(
        "INSERT OR IGNORE INTO pending_deletes
            (entity, uid, deleted_at, peer_uid, label, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            t.entity,
            t.uid,
            t.deleted_at,
            peer_uid,
            label,
            crate::db::now_ms()
        ],
    )?;
    Ok(n)
}

/// Borrados esperando confirmación, para la pantalla de Sync.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingDelete {
    pub id: i64,
    pub entity: String,
    pub uid: String,
    pub peer_uid: String,
    pub label: String,
    pub deleted_at: i64,
}

pub fn pending_deletes(conn: &Connection) -> rusqlite::Result<Vec<PendingDelete>> {
    let mut stmt = conn.prepare(
        "SELECT id, entity, uid, peer_uid, label, deleted_at
         FROM pending_deletes ORDER BY created_at",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(PendingDelete {
            id: r.get(0)?,
            entity: r.get(1)?,
            uid: r.get(2)?,
            peer_uid: r.get(3)?,
            label: r.get(4)?,
            deleted_at: r.get(5)?,
        })
    })?;
    rows.collect()
}

/// Respuesta del usuario a un borrado encolado.
///
/// Aceptar lo aplica (papelera, 30 días). Rechazar borra el tombstone local y
/// deja constancia de la decisión: si no, el otro lado lo vuelve a mandar en
/// el próximo sync y la pregunta se repite para siempre. El otro dispositivo
/// se queda sin el archivo — es lo que pidió; lo que la política protege es
/// esta biblioteca, no la de él.
pub fn resolve_pending_delete(
    conn: &Connection,
    music_dir: &std::path::Path,
    id: i64,
    accept: bool,
) -> rusqlite::Result<usize> {
    let row: Option<(String, String, i64)> = conn
        .query_row(
            "SELECT entity, uid, deleted_at FROM pending_deletes WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((entity, uid, deleted_at)) = row else { return Ok(0) };
    let done = if accept {
        apply_tombstone(
            conn,
            music_dir,
            &Tombstone { entity: entity.clone(), uid: uid.clone(), deleted_at },
        )?
    } else {
        conn.execute(
            "DELETE FROM tombstones WHERE entity = ?1 AND uid = ?2",
            rusqlite::params![entity, uid],
        )?;
        conn.execute(
            "INSERT OR REPLACE INTO delete_ignores (entity, uid, decided_at)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![entity, uid, crate::db::now_ms()],
        )?;
        0
    };
    conn.execute("DELETE FROM pending_deletes WHERE id = ?1", [id])?;
    Ok(done)
}

/// Qué hacer con un borrado que llega de otro dispositivo. Se evalúa del lado
/// que RECIBE: poner `Ask` en la PC principal la protege de un borrado hecho
/// en el celular sin bloquear el resto del sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletePolicy {
    Propagate,
    /// Encolar en `pending_deletes` para que el usuario confirme (la cola se
    /// resuelve desde la pantalla de Sync). El resto del sync no se bloquea.
    Ask,
    Ignore,
}

impl DeletePolicy {
    pub fn from_setting(s: &str) -> Self {
        match s {
            "ignore" => Self::Ignore,
            "ask" => Self::Ask,
            _ => Self::Propagate,
        }
    }
}

/// Aplica un borrado. Devuelve 1 si borró algo, 0 si no había nada que borrar.
///
/// El archivo de audio **no se destruye**: va a la papelera de la biblioteca,
/// donde sobrevive 30 días. Un borrado local lo hiciste vos mirando la
/// pantalla; uno que llega por la red conviene que sea recuperable.
fn apply_tombstone(
    conn: &Connection,
    music_dir: &std::path::Path,
    t: &Tombstone,
) -> rusqlite::Result<usize> {
    match t.entity.as_str() {
        "track" => {
            let row: Option<(i64, String)> = conn
                .query_row(
                    "SELECT id, path FROM tracks WHERE uid = ?1",
                    [&t.uid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((id, path)) = row else { return Ok(0) };
            let path = std::path::Path::new(&path);
            // Sólo se toca lo que vive en la carpeta gestionada. Un archivo
            // legacy de afuera no es nuestro para moverlo.
            if path.starts_with(music_dir) && path.exists() {
                match crate::trash::move_to_trash(music_dir, path) {
                    Ok(dest) => log::info!("[sync] a la papelera: {}", dest.display()),
                    Err(e) => {
                        // Si el archivo no se pudo mover, la fila se queda:
                        // una fila sin archivo es peor que un borrado que no
                        // se aplicó y se reintenta en el próximo sync.
                        log::warn!("[sync] no se pudo mover a la papelera {}: {e}", path.display());
                        return Ok(0);
                    }
                }
            }
            // El CASCADE lo saca de todas las playlists.
            conn.execute("DELETE FROM tracks WHERE id = ?1", [id])?;
            Ok(1)
        }
        "playlist" => {
            let n = conn.execute("DELETE FROM playlists WHERE uid = ?1", [&t.uid])?;
            Ok(n)
        }
        "playlist_track" => {
            let Some((pl, tr)) = t.uid.split_once(':') else { return Ok(0) };
            // Sólo si el borrado es posterior al agregado local. Si acá se
            // volvió a agregar después, ese agregado es la última palabra.
            let n = conn.execute(
                "DELETE FROM playlist_tracks
                 WHERE playlist_id = (SELECT id FROM playlists WHERE uid = ?1)
                   AND track_id = (SELECT id FROM tracks WHERE uid = ?2)
                   AND added_at <= ?3",
                rusqlite::params![pl, tr, t.deleted_at],
            )?;
            Ok(n)
        }
        _ => Ok(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        db::init_schema(&conn).unwrap();
        conn
    }

    fn add_track(conn: &Connection, uid: &str, title: &str, updated_at: i64) -> i64 {
        conn.execute(
            "INSERT INTO tracks (path, uid, title, updated_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![format!("/m/{uid}"), uid, title, updated_at],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn entry(uid: &str, title: &str, updated_at: i64) -> TrackEntry {
        TrackEntry {
            uid: uid.into(),
            hash: Some("h".into()),
            size: 1,
            filename: format!("{uid}.flac"),
            title: title.into(),
            artist: "A".into(),
            album: String::new(),
            genre: String::new(),
            duration_ms: 0,
            bpm: None,
            updated_at,
            present: true,
        }
    }

    fn pl(uid: &str, name: &str, parent: Option<&str>, updated_at: i64) -> PlaylistEntry {
        PlaylistEntry {
            uid: uid.into(),
            name: name.into(),
            kind: if parent.is_none() && uid.starts_with("f") { "folder".into() } else { "playlist".into() },
            parent_uid: parent.map(|s| s.to_string()),
            rank: "V".into(),
            updated_at,
        }
    }

    #[test]
    fn newer_metadata_wins_and_older_is_ignored() {
        let conn = mem();
        add_track(&conn, "t1", "viejo", 100);
        let changes = Changes {
            tracks: vec![entry("t1", "nuevo", 500)],
            ..Default::default()
        };
        assert_eq!(apply(&conn, &changes, std::path::Path::new("/nada"), DeletePolicy::Propagate, "peer").unwrap().tracks, 1);
        let title: String = conn
            .query_row("SELECT title FROM tracks WHERE uid = 't1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(title, "nuevo");

        // Un cambio más viejo no pisa nada.
        let old = Changes {
            tracks: vec![entry("t1", "anterior", 200)],
            ..Default::default()
        };
        assert_eq!(apply(&conn, &old, std::path::Path::new("/nada"), DeletePolicy::Propagate, "peer").unwrap().tracks, 0);
        let title: String = conn
            .query_row("SELECT title FROM tracks WHERE uid = 't1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(title, "nuevo");
    }

    /// Aplicar dos veces lo mismo no puede cambiar nada la segunda vez: si no,
    /// cada sync se contaría como trabajo pendiente para siempre.
    #[test]
    fn applying_twice_is_a_no_op() {
        let conn = mem();
        add_track(&conn, "t1", "viejo", 100);
        let changes = Changes {
            tracks: vec![entry("t1", "nuevo", 500)],
            playlists: vec![pl("p1", "Sets", None, 10)],
            memberships: vec![Membership {
                playlist_uid: "p1".into(),
                track_uid: "t1".into(),
                rank: "V".into(),
                added_at: 0,
            }],
            ..Default::default()
        };
        let first = apply(&conn, &changes, std::path::Path::new("/nada"), DeletePolicy::Propagate, "peer").unwrap();
        assert!(first.total() > 0);
        let second = apply(&conn, &changes, std::path::Path::new("/nada"), DeletePolicy::Propagate, "peer").unwrap();
        assert_eq!(second.total(), 0, "la segunda pasada no debe cambiar nada");
    }

    /// Las carpetas pueden llegar después de sus hijos: la jerarquía tiene que
    /// quedar bien igual.
    #[test]
    fn hierarchy_resolves_regardless_of_arrival_order() {
        let conn = mem();
        let changes = Changes {
            // El hijo primero, el padre después.
            playlists: vec![
                pl("p1", "Warmup", Some("f1"), 10),
                pl("f1", "Electronica", None, 10),
            ],
            ..Default::default()
        };
        apply(&conn, &changes, std::path::Path::new("/nada"), DeletePolicy::Propagate, "peer").unwrap();

        let (child_parent, folder_id): (Option<i64>, i64) = (
            conn.query_row("SELECT parent_id FROM playlists WHERE uid = 'p1'", [], |r| r.get(0))
                .unwrap(),
            conn.query_row("SELECT id FROM playlists WHERE uid = 'f1'", [], |r| r.get(0))
                .unwrap(),
        );
        assert_eq!(child_parent, Some(folder_id));
    }

    /// Una playlist borrada acá no vuelve porque el otro todavía la tenga.
    #[test]
    fn tombstoned_playlists_are_not_recreated() {
        let conn = mem();
        conn.execute(
            "INSERT INTO tombstones (entity, uid, deleted_at) VALUES ('playlist', 'p1', 999)",
            [],
        )
        .unwrap();
        let changes = Changes {
            playlists: vec![pl("p1", "Sets", None, 10)],
            ..Default::default()
        };
        assert_eq!(apply(&conn, &changes, std::path::Path::new("/nada"), DeletePolicy::Propagate, "peer").unwrap().playlists, 0);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM playlists", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    /// Sacar un track de una playlist sólo se propaga con tombstone; sin él,
    /// la unión lo vuelve a poner (agregar le gana a quitar concurrente).
    #[test]
    fn removed_membership_stays_removed_only_with_a_tombstone() {
        let conn = mem();
        add_track(&conn, "t1", "x", 1);
        apply(
            &conn,
            &Changes {
                playlists: vec![pl("p1", "Sets", None, 10)],
                ..Default::default()
            },
            std::path::Path::new("/nada"),
            DeletePolicy::Propagate, "peer",
        )
        .unwrap();
        let member = Changes {
            memberships: vec![Membership {
                playlist_uid: "p1".into(),
                track_uid: "t1".into(),
                rank: "V".into(),
                added_at: 0,
            }],
            ..Default::default()
        };
        assert_eq!(apply(&conn, &member, std::path::Path::new("/nada"), DeletePolicy::Propagate, "peer").unwrap().memberships, 1);

        // Se saca acá, con constancia.
        conn.execute("DELETE FROM playlist_tracks", []).unwrap();
        conn.execute(
            "INSERT INTO tombstones (entity, uid, deleted_at) VALUES ('playlist_track','p1:t1',999)",
            [],
        )
        .unwrap();
        assert_eq!(apply(&conn, &member, std::path::Path::new("/nada"), DeletePolicy::Propagate, "peer").unwrap().memberships, 0, "no debe volver");
    }

    /// Una membresía de un track que todavía no llegó se ignora sin romper
    /// nada; el próximo sync la trae cuando el archivo esté.
    #[test]
    fn membership_of_an_unknown_track_is_skipped() {
        let conn = mem();
        let changes = Changes {
            playlists: vec![pl("p1", "Sets", None, 10)],
            memberships: vec![Membership {
                playlist_uid: "p1".into(),
                track_uid: "todavia-no".into(),
                rank: "V".into(),
                added_at: 0,
            }],
            ..Default::default()
        };
        let applied = apply(&conn, &changes, std::path::Path::new("/nada"), DeletePolicy::Propagate, "peer").unwrap();
        assert_eq!(applied.playlists, 1);
        assert_eq!(applied.memberships, 0);
    }

    /// La metadata de un track cuyo archivo no está no crea una entrada
    /// fantasma: la fila llega junto con el archivo.
    #[test]
    fn metadata_for_an_unknown_track_does_not_create_a_row() {
        let conn = mem();
        let changes = Changes {
            tracks: vec![entry("nunca-visto", "x", 10)],
            ..Default::default()
        };
        assert_eq!(apply(&conn, &changes, std::path::Path::new("/nada"), DeletePolicy::Propagate, "peer").unwrap().tracks, 0);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    fn tmp_music(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sway-merge-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn add_track_at(conn: &Connection, uid: &str, path: &std::path::Path) {
        conn.execute(
            "INSERT INTO tracks (path, uid, title, updated_at) VALUES (?1, ?2, 'x', 1)",
            rusqlite::params![path.to_string_lossy(), uid],
        )
        .unwrap();
    }

    fn tomb(entity: &str, uid: &str) -> Tombstone {
        Tombstone {
            entity: entity.into(),
            uid: uid.into(),
            deleted_at: 1000,
        }
    }

    /// Un borrado que llega por la red saca el track de la biblioteca, pero el
    /// archivo va a la papelera: recuperable, no destruido.
    #[test]
    fn a_propagated_delete_moves_the_file_to_the_trash() {
        let music = tmp_music("del");
        let conn = mem();
        let f = music.join("borrado.flac");
        std::fs::write(&f, b"audio").unwrap();
        add_track_at(&conn, "t1", &f);

        let changes = Changes {
            tombstones: vec![tomb("track", "t1")],
            ..Default::default()
        };
        let applied = apply(&conn, &changes, &music, DeletePolicy::Propagate, "peer").unwrap();

        assert_eq!(applied.deleted, 1);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "sale de la biblioteca");
        assert!(!f.exists(), "y de la carpeta gestionada");
        let recuperable = std::fs::read_dir(crate::trash::trash_dir(&music))
            .unwrap()
            .flatten()
            .any(|e| std::fs::read(e.path()).unwrap() == b"audio");
        assert!(recuperable, "pero sigue existiendo en la papelera");
        std::fs::remove_dir_all(&music).ok();
    }

    /// Con la política en `ignore`, el borrado del otro no toca nada acá —
    /// pero el tombstone igual se guarda, para no devolvérselo.
    #[test]
    fn an_ignored_delete_keeps_the_track_but_records_the_tombstone() {
        let music = tmp_music("ign");
        let conn = mem();
        let f = music.join("queda.flac");
        std::fs::write(&f, b"audio").unwrap();
        add_track_at(&conn, "t1", &f);

        let changes = Changes {
            tombstones: vec![tomb("track", "t1")],
            ..Default::default()
        };
        let applied = apply(&conn, &changes, &music, DeletePolicy::Ignore, "peer").unwrap();

        assert_eq!(applied.deleted, 0);
        assert!(f.exists());
        assert!(has_tombstone(&conn, "track", "t1"), "igual queda constancia");
        std::fs::remove_dir_all(&music).ok();
    }

    /// Con la política en `ask`, el borrado no se aplica solo: queda en la
    /// cola y el archivo sigue donde estaba hasta que alguien decida.
    #[test]
    fn a_protected_delete_waits_in_the_queue() {
        let music = tmp_music("ask");
        let conn = mem();
        let f = music.join("espera.flac");
        std::fs::write(&f, b"audio").unwrap();
        add_track_at(&conn, "t1", &f);

        let changes = Changes {
            tombstones: vec![tomb("track", "t1")],
            ..Default::default()
        };
        let applied = apply(&conn, &changes, &music, DeletePolicy::Ask, "celu").unwrap();

        assert_eq!(applied.deleted, 0);
        assert_eq!(applied.queued, 1);
        assert!(f.exists(), "el archivo no se toca hasta que se confirme");
        let queue = pending_deletes(&conn).unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].peer_uid, "celu");

        // Y un segundo sync con el mismo tombstone no duplica la pregunta.
        apply(&conn, &changes, &music, DeletePolicy::Ask, "celu").unwrap();
        assert_eq!(pending_deletes(&conn).unwrap().len(), 1);
        std::fs::remove_dir_all(&music).ok();
    }

    #[test]
    fn accepting_a_queued_delete_applies_it() {
        let music = tmp_music("ask-ok");
        let conn = mem();
        let f = music.join("chau.flac");
        std::fs::write(&f, b"audio").unwrap();
        add_track_at(&conn, "t1", &f);
        let changes = Changes {
            tombstones: vec![tomb("track", "t1")],
            ..Default::default()
        };
        apply(&conn, &changes, &music, DeletePolicy::Ask, "celu").unwrap();
        let id = pending_deletes(&conn).unwrap()[0].id;

        assert_eq!(resolve_pending_delete(&conn, &music, id, true).unwrap(), 1);
        assert!(!f.exists());
        assert!(pending_deletes(&conn).unwrap().is_empty());
        std::fs::remove_dir_all(&music).ok();
    }

    /// Rechazar tiene que pegarse: sin la constancia, el tombstone del otro
    /// lado vuelve en el próximo sync y la cola pregunta lo mismo para siempre.
    #[test]
    fn rejecting_a_queued_delete_is_remembered() {
        let music = tmp_music("ask-no");
        let conn = mem();
        let f = music.join("me-lo-quedo.flac");
        std::fs::write(&f, b"audio").unwrap();
        add_track_at(&conn, "t1", &f);
        let changes = Changes {
            tombstones: vec![tomb("track", "t1")],
            ..Default::default()
        };
        apply(&conn, &changes, &music, DeletePolicy::Ask, "celu").unwrap();
        let id = pending_deletes(&conn).unwrap()[0].id;

        resolve_pending_delete(&conn, &music, id, false).unwrap();
        assert!(f.exists());
        assert!(!has_tombstone(&conn, "track", "t1"), "el borrado se descarta");

        // El mismo tombstone vuelve a llegar y ya no molesta.
        let applied = apply(&conn, &changes, &music, DeletePolicy::Ask, "celu").unwrap();
        assert_eq!(applied.queued, 0);
        assert!(pending_deletes(&conn).unwrap().is_empty());
        assert!(f.exists());
        std::fs::remove_dir_all(&music).ok();
    }

    /// El scope viaja como cualquier otro dato replicado: lo edito acá para el
    /// celular y el celular se entera.
    #[test]
    fn scope_rows_travel_and_the_newest_wins() {
        let mut local = empty_manifest();
        local.scopes.push(ScopeEntry {
            device_uid: "celu".into(),
            playlist_uid: "sets".into(),
            selected: true,
            updated_at: 500,
        });
        local.device_sync.push(DeviceSync {
            device_uid: "celu".into(),
            mode: "selected".into(),
            direction: "both".into(),
            updated_at: 500,
        });
        let mut remote = empty_manifest();
        remote.scopes.push(ScopeEntry {
            device_uid: "celu".into(),
            playlist_uid: "sets".into(),
            selected: false,
            updated_at: 100,
        });

        let out = changes_for_peer(&local, &remote);
        assert_eq!(out.scopes.len(), 1, "lo mío es más nuevo: viaja");
        assert_eq!(out.device_sync.len(), 1);

        // Y del otro lado no vuelve nada: lo suyo quedó viejo.
        assert!(changes_for_peer(&remote, &local).scopes.is_empty());

        let conn = mem();
        let applied = apply(&conn, &out, std::path::Path::new("/nada"), DeletePolicy::Propagate, "peer").unwrap();
        assert_eq!(applied.scope, 2);
        let s = crate::scope::get(&conn, "celu").unwrap();
        assert_eq!(s.mode, crate::scope::Mode::Selected);
        assert!(s.selected.contains("sets"));
    }

    /// Borrar una playlist no puede llevarse la música puesta.
    #[test]
    fn deleting_a_playlist_does_not_delete_its_tracks() {
        let music = tmp_music("pl");
        let conn = mem();
        let f = music.join("sobrevive.flac");
        std::fs::write(&f, b"audio").unwrap();
        add_track_at(&conn, "t1", &f);
        apply(
            &conn,
            &Changes {
                playlists: vec![pl("p1", "Sets", None, 10)],
                memberships: vec![Membership {
                    playlist_uid: "p1".into(),
                    track_uid: "t1".into(),
                    rank: "V".into(),
                added_at: 0,
                }],
                ..Default::default()
            },
            &music,
            DeletePolicy::Propagate, "peer",
        )
        .unwrap();

        apply(
            &conn,
            &Changes {
                tombstones: vec![tomb("playlist", "p1")],
                ..Default::default()
            },
            &music,
            DeletePolicy::Propagate, "peer",
        )
        .unwrap();

        let playlists: i64 = conn
            .query_row("SELECT COUNT(*) FROM playlists", [], |r| r.get(0))
            .unwrap();
        let tracks: i64 = conn
            .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(playlists, 0);
        assert_eq!(tracks, 1, "el track sigue en la biblioteca");
        assert!(f.exists(), "y su archivo también");
        std::fs::remove_dir_all(&music).ok();
    }

    /// Un archivo de afuera de la carpeta gestionada (legacy) no es nuestro
    /// para moverlo: se saca de la biblioteca y el archivo queda donde está.
    #[test]
    fn a_file_outside_the_managed_folder_is_left_alone() {
        let music = tmp_music("legacy-managed");
        let elsewhere = tmp_music("legacy-outside");
        let conn = mem();
        let f = elsewhere.join("ajeno.flac");
        std::fs::write(&f, b"audio").unwrap();
        add_track_at(&conn, "t1", &f);

        apply(
            &conn,
            &Changes {
                tombstones: vec![tomb("track", "t1")],
                ..Default::default()
            },
            &music,
            DeletePolicy::Propagate, "peer",
        )
        .unwrap();

        assert!(f.exists(), "el archivo ajeno no se toca");
        std::fs::remove_dir_all(&music).ok();
        std::fs::remove_dir_all(&elsewhere).ok();
    }

    fn manifest_with(playlist_updated: i64, rank: &str) -> Manifest {
        Manifest {
            device_uid: "d".into(),
            tracks: vec![entry("t1", "x", 1)],
            playlists: vec![pl("p1", "Sets", None, playlist_updated)],
            memberships: vec![Membership {
                playlist_uid: "p1".into(),
                track_uid: "t1".into(),
                rank: rank.into(),
                added_at: 1,
            }],
            tombstones: vec![],
            ..Default::default()
        }
    }

    /// Reordenar no agrega ni saca nada: el par ya existe de los dos lados y
    /// sólo cambia su rank. Sin esto, mover una canción dentro de una playlist
    /// no se propagaba nunca — el orden de un set es medio el punto.
    #[test]
    fn reordering_travels_when_the_playlist_is_newer() {
        let reordenada = manifest_with(500, "b");
        let vieja = manifest_with(100, "a");

        let c = changes_for_peer(&reordenada, &vieja);
        assert_eq!(c.memberships.len(), 1, "el rank nuevo tiene que viajar");
        assert_eq!(c.memberships[0].rank, "b");

        // Y en la otra dirección no: el que tocó último manda.
        let c = changes_for_peer(&vieja, &reordenada);
        assert!(c.memberships.is_empty());
    }

    /// Mismo orden en los dos lados: nada que mandar, por más que la playlist
    /// tenga fechas distintas.
    #[test]
    fn an_unchanged_order_is_not_resent() {
        let a = manifest_with(500, "a");
        let b = manifest_with(100, "a");
        assert!(changes_for_peer(&a, &b).memberships.is_empty());
    }

    /// Y al aplicar, un par que ya existe tiene que quedar con el rank nuevo.
    #[test]
    fn applying_a_reorder_updates_the_existing_rank() {
        let conn = mem();
        add_track(&conn, "t1", "x", 1);
        let base = Changes {
            playlists: vec![pl("p1", "Sets", None, 10)],
            memberships: vec![Membership {
                playlist_uid: "p1".into(),
                track_uid: "t1".into(),
                rank: "a".into(),
                added_at: 0,
            }],
            ..Default::default()
        };
        let music = std::path::Path::new("/nada");
        apply(&conn, &base, music, DeletePolicy::Propagate, "peer").unwrap();

        let reorder = Changes {
            memberships: vec![Membership {
                playlist_uid: "p1".into(),
                track_uid: "t1".into(),
                rank: "z".into(),
                added_at: 0,
            }],
            ..Default::default()
        };
        assert_eq!(
            apply(&conn, &reorder, music, DeletePolicy::Propagate, "peer")
                .unwrap()
                .memberships,
            1
        );
        let rank: String = conn
            .query_row("SELECT rank FROM playlist_tracks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rank, "z");

        // Y aplicarlo otra vez no cuenta como trabajo.
        assert_eq!(
            apply(&conn, &reorder, music, DeletePolicy::Propagate, "peer")
                .unwrap()
                .memberships,
            0
        );
    }

    fn empty_manifest() -> Manifest {
        Manifest {
            device_uid: "d".into(),
            tracks: vec![],
            playlists: vec![],
            memberships: vec![],
            tombstones: vec![],
            ..Default::default()
        }
    }

    fn member(rank: &str, added_at: i64) -> Membership {
        Membership {
            playlist_uid: "p1".into(),
            track_uid: "t1".into(),
            rank: rank.into(),
            added_at,
        }
    }

    /// Volver a agregar una canción a una playlist de la que se la había
    /// sacado. El tombstone de aquella vez no puede ganarle al agregado
    /// nuevo: si lo hiciera, agregarla se desharía solo en el próximo sync —
    /// que es exactamente lo que pasaba.
    #[test]
    fn re_adding_a_track_beats_an_older_removal() {
        let music = std::path::Path::new("/nada");
        let conn = mem();
        add_track(&conn, "t1", "x", 1);
        apply(
            &conn,
            &Changes {
                playlists: vec![pl("p1", "Sets", None, 10)],
                ..Default::default()
            },
            music,
            DeletePolicy::Propagate, "peer",
        )
        .unwrap();

        // Se sacó hace rato...
        conn.execute(
            "INSERT INTO tombstones (entity, uid, deleted_at) VALUES ('playlist_track','p1:t1',100)",
            [],
        )
        .unwrap();

        // ...y ahora llega un agregado POSTERIOR.
        let re_add = Changes {
            memberships: vec![member("V", 500)],
            ..Default::default()
        };
        assert_eq!(apply(&conn, &re_add, music, DeletePolicy::Propagate, "peer").unwrap().memberships, 1);

        // Y el tombstone viejo se limpia, para no volver a estorbar.
        assert!(!has_tombstone(&conn, "playlist_track", "p1:t1"));

        // Un tombstone POSTERIOR sí gana.
        let removal = Changes {
            tombstones: vec![Tombstone {
                entity: "playlist_track".into(),
                uid: "p1:t1".into(),
                deleted_at: 900,
            }],
            ..Default::default()
        };
        assert_eq!(apply(&conn, &removal, music, DeletePolicy::Propagate, "peer").unwrap().deleted, 1);
    }

    /// Y un borrado que llega viejo no puede sacar algo agregado después.
    #[test]
    fn an_older_removal_does_not_undo_a_newer_add() {
        let music = std::path::Path::new("/nada");
        let conn = mem();
        add_track(&conn, "t1", "x", 1);
        apply(
            &conn,
            &Changes {
                playlists: vec![pl("p1", "Sets", None, 10)],
                memberships: vec![member("V", 800)],
                ..Default::default()
            },
            music,
            DeletePolicy::Propagate, "peer",
        )
        .unwrap();

        let stale = Changes {
            tombstones: vec![Tombstone {
                entity: "playlist_track".into(),
                uid: "p1:t1".into(),
                deleted_at: 200,
            }],
            ..Default::default()
        };
        assert_eq!(apply(&conn, &stale, music, DeletePolicy::Propagate, "peer").unwrap().deleted, 0);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM playlist_tracks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "la canción se queda");
    }

    /// El mismo criterio del lado que decide qué mandar: no se omite un
    /// agregado nuevo por un tombstone viejo del otro.
    #[test]
    fn a_newer_add_is_still_sent_despite_an_older_remote_tombstone() {
        let mut local = empty_manifest();
        local.playlists.push(pl("p1", "Sets", None, 10));
        local.memberships.push(member("V", 500));

        let mut remote = empty_manifest();
        remote.playlists.push(pl("p1", "Sets", None, 10));
        remote.tombstones.push(Tombstone {
            entity: "playlist_track".into(),
            uid: "p1:t1".into(),
            deleted_at: 100,
        });

        assert_eq!(changes_for_peer(&local, &remote).memberships.len(), 1);

        // Pero si el tombstone del otro es posterior, no se manda.
        remote.tombstones[0].deleted_at = 900;
        assert!(changes_for_peer(&local, &remote).memberships.is_empty());
    }

    #[test]
    fn changes_for_peer_only_includes_what_is_newer_or_missing() {
        let local = Manifest {
            device_uid: "a".into(),
            tracks: vec![entry("t1", "nuevo", 500), entry("t2", "igual", 100)],
            playlists: vec![pl("p1", "Sets", None, 50)],
            memberships: vec![Membership {
                playlist_uid: "p1".into(),
                track_uid: "t1".into(),
                rank: "V".into(),
                added_at: 0,
            }],
            tombstones: vec![],
            ..Default::default()
        };
        let remote = Manifest {
            device_uid: "b".into(),
            tracks: vec![entry("t1", "viejo", 100), entry("t2", "igual", 100)],
            playlists: vec![],
            memberships: vec![],
            tombstones: vec![],
            ..Default::default()
        };
        let c = changes_for_peer(&local, &remote);
        assert_eq!(c.tracks.len(), 1, "sólo t1, que es más nuevo");
        assert_eq!(c.tracks[0].uid, "t1");
        assert_eq!(c.playlists.len(), 1);
        assert_eq!(c.memberships.len(), 1);
    }
}
