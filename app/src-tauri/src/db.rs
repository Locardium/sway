use rusqlite::{Connection, OptionalExtension, Result};
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
    /// El archivo esta en este dispositivo. `false` = la fila quedo pero el
    /// blob se evacuo por sync selectiva (Fase 5.7), o todavia no se bajo.
    pub present: bool,
    /// Entra en lo que este dispositivo sincroniza. `false` = su playlist no
    /// esta marcada: el archivo que ya esta no se toca, pero no se va a bajar
    /// ni a actualizar. Lo completa el comando, no la query — depende del
    /// scope, no de la fila.
    ///
    /// Junto con `present` decide como se ve: fuera de scope y con archivo se
    /// muestra apagado y no suena; fuera de scope y sin archivo desaparece de
    /// la vista principal (ya se libero el espacio).
    pub in_scope: bool,
    pub uid: Option<String>,
}

const SCHEMA: &str = "
-- Identidad de un track: DOS columnas, no una.
--   `uid`          identidad logica (UUID). Sobrevive retag, renombre, movida.
--                  Es lo que referencian playlists, cues y tombstones, y lo
--                  unico que significa algo en el otro dispositivo (el `id`
--                  INTEGER es local: el 42 de esta maquina no es el 42 de la
--                  otra).
--   `content_hash` identidad de los bytes (blake3). Es lo que se pide y se
--                  verifica en una transferencia, y lo que permite deduplicar
--                  el mismo archivo importado dos veces por separado.
-- Separadas porque editar el genero cambia metadata pero no bytes: mismo uid,
-- mismo hash, clock nuevo. (Corolario: Sway NO reescribe tags dentro de los
-- archivos; si alguna vez lo hiciera habria que rehashear y propagar el blob.)
CREATE TABLE IF NOT EXISTS tracks (
    id          INTEGER PRIMARY KEY,
    path        TEXT NOT NULL UNIQUE,
    title       TEXT NOT NULL DEFAULT '',
    artist      TEXT NOT NULL DEFAULT '',
    album       TEXT NOT NULL DEFAULT '',
    genre       TEXT NOT NULL DEFAULT '',
    duration_ms INTEGER NOT NULL DEFAULT 0,
    bpm         INTEGER,
    uid          TEXT,
    content_hash TEXT,
    -- Nombre del archivo dentro de la carpeta gestionada. `path` es absoluto y
    -- por lo tanto local; lo que viaja entre dispositivos es esto.
    rel_path     TEXT,
    size_bytes   INTEGER,
    -- (size, mtime) son el cache del hash: si no cambiaron, no se rehashea.
    mtime_ms     INTEGER,
    -- LWW por campo: {\"artist\": [ts_ms, device_uid], ...}. Se puebla cuando
    -- exista edicion de metadata (Fase 5.5); hoy queda NULL.
    field_clocks TEXT,
    updated_at   INTEGER NOT NULL DEFAULT 0,
    -- present  = el archivo esta en este dispositivo
    -- absent   = la fila esta, el blob no (sync selectiva: el celu conoce el
    --            track pero no se lo bajo)
    -- pending  = transferencia en curso
    local_state  TEXT NOT NULL DEFAULT 'present'
);
-- Los indices sobre uid/content_hash los crea migrate(): en una base vieja
-- esas columnas todavia no existen cuando corre este batch.

-- Jerarquia virtual (folders + playlists) — se usa desde Fase 2/3.
-- `rank` es un rank fraccional (ver rank.rs), no un indice: reordenar toca
-- una sola fila, asi que dos dispositivos que reordenan offline mergean sin
-- pisarse. El orden es `ORDER BY rank` (colacion BINARY).
CREATE TABLE IF NOT EXISTS playlists (
    id        INTEGER PRIMARY KEY,
    name      TEXT NOT NULL,
    kind      TEXT NOT NULL DEFAULT 'playlist',  -- 'folder' | 'playlist'
    parent_id INTEGER REFERENCES playlists(id) ON DELETE CASCADE,
    rank      TEXT NOT NULL DEFAULT '',
    uid        TEXT,
    updated_at INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS playlist_tracks (
    playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    track_id    INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    rank        TEXT NOT NULL DEFAULT '',
    -- Cuando se agrego. Se compara contra `tombstones.deleted_at` del mismo
    -- par: sin esto, un tombstone viejo le gana para siempre a un agregado
    -- nuevo y volver a meter una cancion en una playlist se deshacia solo en
    -- el proximo sync.
    added_at    INTEGER NOT NULL DEFAULT 0,
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

-- Config persistida clave/valor (ej. toggle de auto-sync XML).
CREATE TABLE IF NOT EXISTS app_settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- ---------------------------------------------------------------------------
-- Fase 5: sync P2P.
--
-- `devices`, `sync_policy`, `pending_deletes` y `delete_ignores` son LOCALES:
-- describen con quien sincroniza ESTE dispositivo y bajo que reglas. La
-- politica de borrados en particular PROTEGE a este dispositivo; si se pudiera
-- cambiar desde el otro lado no protegeria de nada.
--
-- `device_scope` y `sync_scope` en cambio SI se replican (Fase 5.7): el scope
-- describe un deseo (quiero estas playlists en el celular), no una regla de
-- seguridad, y se edita desde cualquier dispositivo.
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS devices (
    uid          TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    platform     TEXT NOT NULL DEFAULT '',
    pubkey       BLOB,               -- clave estatica Noise, fijada al parear
    paired_at    INTEGER,
    last_seen    INTEGER,
    last_sync_at INTEGER             -- corte del manifest incremental
);

-- Lo unico de a pares y local que queda: que hago con los borrados que me
-- manda ESE dispositivo. La direccion se mudo a `device_sync` (es una
-- propiedad del dispositivo, no del vinculo).
CREATE TABLE IF NOT EXISTS sync_policy (
    device_uid TEXT PRIMARY KEY REFERENCES devices(uid) ON DELETE CASCADE,
    -- Se evalua del lado que RECIBE el tombstone: 'propagate' aplica el
    -- borrado, 'ask' lo encola para que el usuario confirme, 'ignore' lo
    -- descarta. Poner 'ask' en la PC principal la protege de un borrado
    -- hecho en otro dispositivo sin bloquear el resto del sync.
    deletes    TEXT NOT NULL DEFAULT 'propagate'
);

-- Preferencias de sync de CADA dispositivo (incluido este). Replicadas, LWW
-- por `updated_at`. Vive aparte de `sync_policy` a proposito: la politica es
-- de a pares y local (que hago yo con lo que me manda ese), esto es una
-- propiedad del dispositivo (que hace, y que playlists viven ahi) y la misma
-- respuesta vale para todos los que la miren.
--
--   `direction`  que hace ESE dispositivo: manda, recibe, las dos, o nada.
--                Entre dos dispositivos, A -> B pasa solo si A manda Y B
--                recibe. No hace falta negociar nada por la red: los dos
--                lados leen las mismas dos filas.
--   `mode`       'all' o 'selected' (ver sync_scope).
CREATE TABLE IF NOT EXISTS device_sync (
    device_uid TEXT PRIMARY KEY,
    mode       TEXT NOT NULL DEFAULT 'all',    -- all|selected
    direction  TEXT NOT NULL DEFAULT 'both',   -- both|send|receive|off
    updated_at INTEGER NOT NULL DEFAULT 0
);

-- Que playlists/carpetas quiere cada dispositivo. Replicada, LWW por fila.
--
-- Desmarcar NO borra la fila: la deja con `selected = 0`. Con la fila borrada,
-- la union del merge la volveria a traer del otro lado en el proximo sync y
-- desmarcar no se pegaria nunca.
CREATE TABLE IF NOT EXISTS sync_scope (
    device_uid   TEXT NOT NULL,
    playlist_uid TEXT NOT NULL,
    selected     INTEGER NOT NULL DEFAULT 1,
    updated_at   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (device_uid, playlist_uid)
);

-- Que dispositivos tienen (o tenian) cada blob. Es lo unico que permite
-- liberar espacio sin arriesgar la ultima copia: un archivo fuera de scope
-- solo se evacua si consta que vive en otro lado.
CREATE TABLE IF NOT EXISTS blob_replicas (
    hash       TEXT NOT NULL,
    device_uid TEXT NOT NULL,
    seen_at    INTEGER NOT NULL,
    PRIMARY KEY (hash, device_uid)
);

-- Borrados entrantes esperando confirmacion (politica `deletes = 'ask'`).
-- Local: es la cola de este dispositivo.
CREATE TABLE IF NOT EXISTS pending_deletes (
    id         INTEGER PRIMARY KEY,
    entity     TEXT NOT NULL,
    uid        TEXT NOT NULL,
    deleted_at INTEGER NOT NULL,
    peer_uid   TEXT NOT NULL DEFAULT '',
    label      TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    UNIQUE (entity, uid)
);

-- Borrados que el usuario rechazo. Sin esto, el tombstone del otro lado
-- vuelve en cada sync y la cola pregunta lo mismo para siempre.
CREATE TABLE IF NOT EXISTS delete_ignores (
    entity     TEXT NOT NULL,
    uid        TEXT NOT NULL,
    decided_at INTEGER NOT NULL,
    PRIMARY KEY (entity, uid)
);

-- Un borrado sin tombstone es un borrado que el proximo sync deshace: el otro
-- dispositivo todavia tiene la fila y te la manda de vuelta, para siempre.
CREATE TABLE IF NOT EXISTS tombstones (
    entity     TEXT NOT NULL,   -- 'track'|'playlist'|'playlist_track'|'cue'
    uid        TEXT NOT NULL,   -- playlist_track: '<playlist_uid>:<track_uid>'
    deleted_at INTEGER NOT NULL,
    device_uid TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (entity, uid)
);

-- Historial legible: dry-runs, conflictos resueltos automaticamente (con el
-- valor perdedor, para poder revertirlo) y transferencias.
CREATE TABLE IF NOT EXISTS sync_log (
    id     INTEGER PRIMARY KEY,
    ts     INTEGER NOT NULL,
    peer   TEXT NOT NULL DEFAULT '',
    kind   TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT ''
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
    init_schema(&conn)?;
    Ok(conn)
}

/// Crea el schema y aplica las migraciones sobre una conexion ya abierta.
/// Lo usan `open` y los tests de otros modulos, para que nadie tenga que
/// mantener una copia del DDL al lado.
pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    migrate(conn)
}

fn has_table(conn: &Connection, table: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let name: String = r.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Migraciones sobre bases ya existentes. `CREATE TABLE IF NOT EXISTS` no
/// toca una tabla que ya esta, asi que las columnas nuevas se agregan aca.
/// Idempotente: en una base recien creada SCHEMA ya las trae y no hace nada.
fn migrate(conn: &Connection) -> Result<()> {
    // `position` INTEGER -> `rank` fraccional (ver rank.rs). La columna vieja
    // se deja donde esta (tiene DEFAULT, no molesta a los INSERT nuevos);
    // sacarla obligaria a reconstruir la tabla entera sin ganancia.
    for (table, group_by) in [("playlists", "parent_id"), ("playlist_tracks", "playlist_id")] {
        if has_column(conn, table, "rank")? {
            continue;
        }
        conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN rank TEXT NOT NULL DEFAULT ''"
        ))?;
        backfill_ranks(conn, table, group_by)?;
    }

    // Columnas de identidad/sync. En una base nueva ya vienen de SCHEMA.
    let added: &[(&str, &str)] = &[
        ("tracks", "uid TEXT"),
        ("tracks", "content_hash TEXT"),
        ("tracks", "rel_path TEXT"),
        ("tracks", "size_bytes INTEGER"),
        ("tracks", "mtime_ms INTEGER"),
        ("tracks", "field_clocks TEXT"),
        ("tracks", "updated_at INTEGER NOT NULL DEFAULT 0"),
        ("tracks", "local_state TEXT NOT NULL DEFAULT 'present'"),
        ("playlists", "uid TEXT"),
        ("playlists", "updated_at INTEGER NOT NULL DEFAULT 0"),
        ("playlist_tracks", "added_at INTEGER NOT NULL DEFAULT 0"),
        // Fase 5.7: el scope pasa a ser dato replicado. En una base de 5.0-5.6
        // la tabla existe con dos columnas y ninguna fila util.
        ("sync_scope", "selected INTEGER NOT NULL DEFAULT 1"),
        ("sync_scope", "updated_at INTEGER NOT NULL DEFAULT 0"),
    ];
    for (table, decl) in added {
        // Una tabla que todavia no existe no necesita migracion: la crea
        // SCHEMA. `migrate` tambien corre sola en los tests sobre bases
        // armadas a mano, y ahi puede faltar.
        if !has_table(conn, table)? {
            continue;
        }
        let name = decl.split(' ').next().unwrap_or_default();
        if !has_column(conn, table, name)? {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {decl}"))?;
        }
    }
    // `device_scope` se llamaba asi cuando solo guardaba el scope; ahora
    // guarda tambien la direccion, que no es scope. Se copia y se tira.
    if has_table(conn, "device_scope")? {
        conn.execute_batch(
            "INSERT OR IGNORE INTO device_sync (device_uid, mode, direction, updated_at)
                SELECT device_uid, mode, 'both', updated_at FROM device_scope;
             DROP TABLE device_scope;",
        )?;
    }

    // Los indices unicos de uid van despues del ALTER (SCHEMA corre antes que
    // exista la columna en una base vieja, asi que ese CREATE INDEX falla y se
    // saltea; aca ya se puede).
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_tracks_uid ON tracks(uid) WHERE uid IS NOT NULL;
         CREATE INDEX IF NOT EXISTS idx_tracks_hash ON tracks(content_hash);
         CREATE UNIQUE INDEX IF NOT EXISTS idx_playlists_uid ON playlists(uid) WHERE uid IS NOT NULL;
         -- La PK de playlist_tracks es (playlist_id, track_id): sirve para ir
         -- de una playlist a sus temas, no al reves. Todo lo del scope va al
         -- reves —de un tema a las playlists que lo contienen— y sin esto cada
         -- una de esas consultas escanea la tabla entera.
         CREATE INDEX IF NOT EXISTS idx_playlist_tracks_track ON playlist_tracks(track_id);
         CREATE INDEX IF NOT EXISTS idx_tracks_state ON tracks(local_state);",
    )?;
    assign_missing_uids(conn)?;
    // Las membresias que ya estaban se fechan AHORA, no en 0: si quedaran en
    // 0, cualquier tombstone viejo del mismo par les ganaria y el proximo
    // sync las sacaria. Estan en la biblioteca ahora, asi que valen ahora.
    conn.execute(
        "UPDATE playlist_tracks SET added_at = ?1 WHERE added_at = 0",
        [now_ms()],
    )?;
    Ok(())
}

/// Toda fila sin `uid` recibe uno. Corre en cada arranque, no solo en la
/// migracion: una fila insertada por un camino que se olvide de generarlo
/// (import, drop del OS, watcher) queda sincronizable igual.
pub fn assign_missing_uids(conn: &Connection) -> Result<()> {
    for table in ["tracks", "playlists"] {
        let ids: Vec<i64> = {
            let mut stmt = conn.prepare(&format!("SELECT id FROM {table} WHERE uid IS NULL"))?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect::<Result<_>>()?
        };
        for id in ids {
            conn.execute(
                &format!("UPDATE {table} SET uid = ?1 WHERE id = ?2"),
                rusqlite::params![new_uid(), id],
            )?;
        }
    }
    Ok(())
}

pub fn new_uid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Milisegundos desde epoch. El proyecto no usa `chrono` (la fecha del XML
/// tambien esta hecha a mano en export_xml.rs), asi que va con `SystemTime`.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Traduce el orden `position` existente a ranks, grupo por grupo, sin alterar
/// el orden que el usuario ya veia.
fn backfill_ranks(conn: &Connection, table: &str, group_by: &str) -> Result<()> {
    let key = if table == "playlists" { "id" } else { "track_id" };
    let groups: Vec<Option<i64>> = {
        let mut stmt = conn.prepare(&format!("SELECT DISTINCT {group_by} FROM {table}"))?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<Result<_>>()?
    };
    for g in groups {
        let ids: Vec<i64> = {
            let mut stmt = conn.prepare(&format!(
                "SELECT {key} FROM {table} WHERE {group_by} IS ?1 ORDER BY position, {key}"
            ))?;
            let rows = stmt.query_map([g], |r| r.get(0))?;
            rows.collect::<Result<_>>()?
        };
        for (id, rank) in ids.iter().zip(crate::rank::initial_ranks(ids.len())) {
            conn.execute(
                &format!("UPDATE {table} SET rank = ?1 WHERE {group_by} IS ?2 AND {key} = ?3"),
                rusqlite::params![rank, g, id],
            )?;
        }
    }
    Ok(())
}

pub fn list_tracks(conn: &Connection) -> Result<Vec<Track>> {
    let mut stmt = conn.prepare(
        "SELECT id, path, title, artist, album, genre, duration_ms, bpm, local_state, uid
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
            present: r.get::<_, String>(8)? == "present",
            uid: r.get(9)?,
            in_scope: true,
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
    // Junta paths y uids antes de borrar de la DB.
    let mut paths = Vec::new();
    let mut uids = Vec::new();
    for id in ids {
        if let Ok(p) = track_path(conn, *id) {
            paths.push(p);
        }
        if let Ok(Some(u)) = track_uid(conn, *id) {
            uids.push(u);
        }
    }
    let tx = conn.transaction()?;
    for id in ids {
        tx.execute("DELETE FROM tracks WHERE id = ?1", [id])?;
    }
    tx.commit()?;
    // El tombstone es lo que impide que el proximo sync lo resucite.
    for uid in uids {
        record_tombstone(conn, "track", &uid)?;
    }
    for p in paths {
        let path = std::path::Path::new(&p);
        if path.starts_with(managed) && path.exists() {
            // El crate `trash` no soporta Android (sin papelera de OS ahi).
            // best-effort: no romper si falla.
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            {
                let _ = trash::delete(path);
            }
            #[cfg(any(target_os = "android", target_os = "ios"))]
            {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Config persistida (app_settings)
// ---------------------------------------------------------------------------

const SETTING_AUTO_SYNC_XML: &str = "auto_sync_xml";
const SETTING_DEVICE_UID: &str = "device_uid";
const SETTING_DEVICE_NAME: &str = "device_name";

pub fn get_setting(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row("SELECT value FROM app_settings WHERE key = ?1", [key], |r| r.get(0))
        .optional()
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO app_settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

/// Identidad de ESTE dispositivo, estable de por vida (se genera una sola vez
/// y queda en `app_settings`). Es lo que firma tombstones y clocks, y lo que
/// desempata los LWW empatados — por eso no puede regenerarse en cada arranque.
pub fn this_device_uid(conn: &Connection) -> Result<String> {
    if let Some(uid) = get_setting(conn, SETTING_DEVICE_UID)? {
        if !uid.is_empty() {
            return Ok(uid);
        }
    }
    let uid = new_uid();
    set_setting(conn, SETTING_DEVICE_UID, &uid)?;
    Ok(uid)
}

/// Nombre visible de este dispositivo. Si lo guardado es un placeholder de
/// una version anterior (el celu se llamaba literalmente "Android") se
/// recalcula: el nombre real recien se puede averiguar desde que existe
/// `device_info`, y si no, el telefono se quedaria con ese nombre para
/// siempre.
pub fn device_name(conn: &Connection) -> Result<String> {
    if let Some(n) = get_setting(conn, SETTING_DEVICE_NAME)? {
        let n = n.trim().to_string();
        if !n.is_empty() && !crate::device_info::PLACEHOLDERS.contains(&n.as_str()) {
            return Ok(n);
        }
    }
    let n = crate::device_info::default_device_name();
    set_setting(conn, SETTING_DEVICE_NAME, &n)?;
    Ok(n)
}

pub fn set_device_name(conn: &Connection, name: &str) -> Result<()> {
    set_setting(conn, SETTING_DEVICE_NAME, name)
}

/// Default true: el punto central de Fase 2 es que el XML se mantenga
/// sincronizado solo, sin que el user tenga que prenderlo a mano.
pub fn get_auto_sync_xml(conn: &Connection) -> Result<bool> {
    let v: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = ?1",
            [SETTING_AUTO_SYNC_XML],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v.map(|s| s == "1").unwrap_or(true))
}

pub fn set_auto_sync_xml(conn: &Connection, enabled: bool) -> Result<()> {
    conn.execute(
        "INSERT INTO app_settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![SETTING_AUTO_SYNC_XML, if enabled { "1" } else { "0" }],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Playlists / folders (jerarquia virtual)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistNode {
    pub id: i64,
    /// Identidad compartida entre dispositivos. La necesita el editor de scope
    /// (Fase 5.7): el `id` INTEGER es local y no significa nada del otro lado.
    pub uid: Option<String>,
    pub name: String,
    pub kind: String, // 'folder' | 'playlist'
    pub parent_id: Option<i64>,
    /// Indice del nodo entre sus hermanos, derivado del `rank`. El frontend
    /// ordena por este numero; el rank en si no sale de Rust.
    pub position: i64,
    pub track_count: i64,
    /// De cuantos de esos tracks hay archivo en este dispositivo. Es lo que se
    /// ve al abrir una playlist desmarcada: mientras el archivo este, el tema
    /// se muestra apagado, sin importar por que sigue estando.
    pub present_count: i64,
    /// Cuantos de esos tracks siguen ocupando lugar POR ESTA playlist: tienen
    /// archivo aca y no entran en el scope de este dispositivo. Es lo que separa
    /// "desmarcada pero todavia ocupa lugar" (sigue en el arbol, apagada) de
    /// "desmarcada y ya liberada" (desaparece).
    ///
    /// Distinto de `present_count` a proposito: un tema que ademas esta en una
    /// playlist marcada se sigue VIENDO aca, pero no cuenta para decidir si esta
    /// playlist sigue existiendo. Su archivo lo sostiene la otra, asi que esta
    /// no lo va a soltar nunca y contarlo la dejaba visible para siempre.
    ///
    /// Lo completa el comando, no la query — depende del scope.
    pub stranded_count: i64,
    /// Esta playlist entra en lo que este dispositivo sincroniza. Lo completa
    /// el comando, no la query — depende del scope, no de la fila.
    pub in_scope: bool,
}

pub fn list_playlists(conn: &Connection) -> Result<Vec<PlaylistNode>> {
    let mut stmt = conn.prepare(
        // Los dos conteos salen de UNA pasada agrupada, no de dos subconsultas
        // correlacionadas por fila: asi eran 2N recorridos de playlist_tracks
        // por cada refresco del arbol, y el arbol se refresca en cada cambio de
        // la biblioteca.
        "SELECT p.id, p.uid, p.name, p.kind, p.parent_id,
                COALESCE(c.total, 0), COALESCE(c.presentes, 0)
         FROM playlists p
         LEFT JOIN (
             SELECT pt.playlist_id AS pid,
                    COUNT(*) AS total,
                    COUNT(CASE WHEN t.local_state = 'present' THEN 1 END) AS presentes
               FROM playlist_tracks pt
               JOIN tracks t ON t.id = pt.track_id
              GROUP BY pt.playlist_id
         ) c ON c.pid = p.id
         ORDER BY p.parent_id, p.rank",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(PlaylistNode {
            id: r.get(0)?,
            uid: r.get(1)?,
            name: r.get(2)?,
            kind: r.get(3)?,
            parent_id: r.get(4)?,
            position: 0,
            track_count: r.get(5)?,
            present_count: r.get(6)?,
            stranded_count: 0,
            in_scope: true,
        })
    })?;
    let mut nodes: Vec<PlaylistNode> = rows.collect::<Result<_>>()?;
    // Ya vienen agrupados por parent y ordenados por rank: numerar de cero
    // dentro de cada grupo.
    let mut seen: std::collections::HashMap<Option<i64>, i64> = std::collections::HashMap::new();
    for n in nodes.iter_mut() {
        let slot = seen.entry(n.parent_id).or_insert(0);
        n.position = *slot;
        *slot += 1;
    }
    Ok(nodes)
}

/// Ranks de los hijos de `parent`, en orden.
fn sibling_ranks(conn: &Connection, parent: Option<i64>, exclude: Option<i64>) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT rank FROM playlists WHERE parent_id IS ?1 AND id IS NOT ?2 ORDER BY rank",
    )?;
    let rows = stmt.query_map(rusqlite::params![parent, exclude], |r| r.get(0))?;
    rows.collect()
}

pub fn create_playlist(
    conn: &Connection,
    name: &str,
    kind: &str,
    parent_id: Option<i64>,
) -> Result<i64> {
    let last: Option<String> = conn.query_row(
        "SELECT MAX(rank) FROM playlists WHERE parent_id IS ?1",
        [parent_id],
        |r| r.get(0),
    )?;
    let rank = crate::rank::between(last.as_deref(), None);
    conn.execute(
        "INSERT INTO playlists (name, kind, parent_id, rank, uid, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![name, kind, parent_id, rank, new_uid(), now_ms()],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn rename_playlist(conn: &Connection, id: i64, name: &str) -> Result<()> {
    conn.execute(
        "UPDATE playlists SET name = ?1, updated_at = ?2 WHERE id = ?3",
        rusqlite::params![name, now_ms(), id],
    )?;
    Ok(())
}

pub fn delete_playlist(conn: &Connection, id: i64) -> Result<()> {
    // Los hijos los borra ON DELETE CASCADE sin pasar por aca, asi que sus
    // uids hay que juntarlos ANTES: un borrado sin tombstone se lo lleva
    // puesto el proximo sync (el otro dispositivo todavia lo tiene y lo
    // reenvia).
    let mut pending = vec![id];
    let mut uids: Vec<String> = Vec::new();
    while let Some(cur) = pending.pop() {
        if let Some(uid) = playlist_uid(conn, cur)? {
            uids.push(uid);
        }
        let mut stmt = conn.prepare("SELECT id FROM playlists WHERE parent_id = ?1")?;
        let kids = stmt.query_map([cur], |r| r.get::<_, i64>(0))?;
        for k in kids {
            pending.push(k?);
        }
    }
    conn.execute("DELETE FROM playlists WHERE id = ?1", [id])?;
    for uid in uids {
        record_tombstone(conn, "playlist", &uid)?;
    }
    Ok(())
}

fn playlist_uid(conn: &Connection, id: i64) -> Result<Option<String>> {
    conn.query_row("SELECT uid FROM playlists WHERE id = ?1", [id], |r| r.get(0))
        .optional()
        .map(|v: Option<Option<String>>| v.flatten())
}

fn track_uid(conn: &Connection, id: i64) -> Result<Option<String>> {
    conn.query_row("SELECT uid FROM tracks WHERE id = ?1", [id], |r| r.get(0))
        .optional()
        .map(|v: Option<Option<String>>| v.flatten())
}

/// Marca una entidad como borrada. `INSERT OR REPLACE` para que un
/// re-borrado refresque el timestamp en vez de fallar.
pub fn record_tombstone(conn: &Connection, entity: &str, uid: &str) -> Result<()> {
    let device = this_device_uid(conn)?;
    conn.execute(
        "INSERT OR REPLACE INTO tombstones (entity, uid, deleted_at, device_uid)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![entity, uid, now_ms(), device],
    )?;
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
        // Hermanos del destino sin el nodo movido: el rank nuevo sale de los
        // dos vecinos del hueco. Ninguna otra fila se toca.
        let siblings = sibling_ranks(&tx, new_parent, Some(id)).map_err(|e| e.to_string())?;
        let rank = crate::rank::rank_at(&siblings, index.max(0) as usize);
        tx.execute(
            "UPDATE playlists SET parent_id = ?1, rank = ?2, updated_at = ?3 WHERE id = ?4",
            rusqlite::params![new_parent, rank, now_ms(), id],
        )
        .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Tracks dentro de una playlist
// ---------------------------------------------------------------------------

/// Ids de las playlists que contienen el track.
pub fn track_playlists(conn: &Connection, track_id: i64) -> Result<Vec<i64>> {
    let mut stmt =
        conn.prepare("SELECT playlist_id FROM playlist_tracks WHERE track_id = ?1")?;
    let rows = stmt.query_map([track_id], |r| r.get(0))?;
    rows.collect()
}

pub fn playlist_tracks(conn: &Connection, playlist_id: i64) -> Result<Vec<Track>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.path, t.title, t.artist, t.album, t.genre, t.duration_ms, t.bpm,
                t.local_state, t.uid
         FROM playlist_tracks pt JOIN tracks t ON t.id = pt.track_id
         WHERE pt.playlist_id = ?1
         ORDER BY pt.rank",
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
            present: r.get::<_, String>(8)? == "present",
            uid: r.get(9)?,
            in_scope: true,
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
    let mut last: Option<String> = tx.query_row(
        "SELECT MAX(rank) FROM playlist_tracks WHERE playlist_id = ?1",
        [playlist_id],
        |r| r.get(0),
    )?;
    for tid in track_ids {
        let rank = crate::rank::between(last.as_deref(), None);
        let n = tx.execute(
            "INSERT OR IGNORE INTO playlist_tracks (playlist_id, track_id, rank, added_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![playlist_id, tid, rank, now_ms()],
        )?;
        // Solo avanza si entro: un duplicado ignorado no consume rank.
        if n > 0 {
            last = Some(rank);
        }
        added += n;
    }
    touch_playlist(&tx, playlist_id)?;
    tx.commit()?;
    // Volver a agregar un track que se habia sacado tiene que levantar su
    // tombstone; si no, el par queda muerto para siempre y el proximo sync lo
    // vuelve a sacar.
    if let Some(pu) = playlist_uid(conn, playlist_id)? {
        for tid in track_ids {
            if let Some(tu) = track_uid(conn, *tid)? {
                conn.execute(
                    "DELETE FROM tombstones WHERE entity = 'playlist_track' AND uid = ?1",
                    [format!("{pu}:{tu}")],
                )?;
            }
        }
    }
    Ok(added)
}

pub fn remove_tracks_from_playlist(
    conn: &mut Connection,
    playlist_id: i64,
    track_ids: &[i64],
) -> Result<()> {
    let pl_uid = playlist_uid(conn, playlist_id)?;
    let mut pairs = Vec::new();
    if let Some(pu) = &pl_uid {
        for tid in track_ids {
            if let Some(tu) = track_uid(conn, *tid)? {
                pairs.push(format!("{pu}:{tu}"));
            }
        }
    }
    let tx = conn.transaction()?;
    for tid in track_ids {
        tx.execute(
            "DELETE FROM playlist_tracks WHERE playlist_id = ?1 AND track_id = ?2",
            rusqlite::params![playlist_id, tid],
        )?;
    }
    // Sin reindexado: los ranks de los que quedan siguen siendo validos y
    // ordenados entre si. Sacar del medio no le mueve el rank a nadie.
    touch_playlist(&tx, playlist_id)?;
    tx.commit()?;
    // La membresia mergea por union (agregar le gana a quitar concurrente),
    // asi que sacar un track solo se propaga si queda constancia explicita.
    for p in pairs {
        record_tombstone(conn, "playlist_track", &p)?;
    }
    Ok(())
}

/// Mueve un bloque de tracks (en su orden actual) al indice `index`.
pub fn reorder_playlist_tracks(
    conn: &mut Connection,
    playlist_id: i64,
    track_ids: &[i64],
    index: i64,
) -> Result<()> {
    let tx = conn.transaction()?;
    let current: Vec<(i64, String)> = {
        let mut stmt = tx.prepare(
            "SELECT track_id, rank FROM playlist_tracks WHERE playlist_id = ?1 ORDER BY rank",
        )?;
        let rows = stmt.query_map([playlist_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<_>>()?
    };
    let moving: Vec<i64> = current
        .iter()
        .map(|(t, _)| *t)
        .filter(|t| track_ids.contains(t))
        .collect();
    // El indice pedido es sobre la lista completa; hay que descontar los
    // elementos movidos que estaban antes del destino.
    let before = current
        .iter()
        .take((index.max(0) as usize).min(current.len()))
        .filter(|(t, _)| track_ids.contains(t))
        .count();
    let staying: Vec<String> = current
        .iter()
        .filter(|(t, _)| !track_ids.contains(t))
        .map(|(_, r)| r.clone())
        .collect();
    let idx = ((index.max(0) as usize).saturating_sub(before)).min(staying.len());
    // Solo se reescriben los ranks del bloque movido; los que se quedan
    // conservan el suyo, que es lo que hace que el merge no pise nada.
    let mut prev = if idx == 0 { None } else { staying.get(idx - 1).cloned() };
    let next = staying.get(idx).cloned();
    for t in moving.iter() {
        let rank = crate::rank::between(prev.as_deref(), next.as_deref());
        tx.execute(
            "UPDATE playlist_tracks SET rank = ?1 WHERE playlist_id = ?2 AND track_id = ?3",
            rusqlite::params![rank, playlist_id, t],
        )?;
        prev = Some(rank);
    }
    touch_playlist(&tx, playlist_id)?;
    tx.commit()
}

/// Marca la playlist como modificada. Es el reloj que usa el merge para
/// decidir de quien es el orden bueno cuando los dos lados reordenaron: las
/// membresias no tienen timestamp propio, la playlist si.
fn touch_playlist(tx: &rusqlite::Transaction, playlist_id: i64) -> Result<()> {
    tx.execute(
        "UPDATE playlists SET updated_at = ?1 WHERE id = ?2",
        rusqlite::params![now_ms(), playlist_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    fn add_track(conn: &Connection, path: &str) -> i64 {
        conn.execute(
            "INSERT INTO tracks (path, uid) VALUES (?1, ?2)",
            rusqlite::params![path, new_uid()],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn tombstones_of(conn: &Connection, entity: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT uid FROM tombstones WHERE entity = ?1 ORDER BY uid")
            .unwrap();
        let rows = stmt.query_map([entity], |r| r.get(0)).unwrap();
        rows.collect::<Result<_>>().unwrap()
    }

    fn order_of(conn: &Connection, pid: i64) -> Vec<i64> {
        playlist_tracks(conn, pid).unwrap().iter().map(|t| t.id).collect()
    }

    fn ranks_of(conn: &Connection, pid: i64) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT rank FROM playlist_tracks WHERE playlist_id = ?1 ORDER BY rank")
            .unwrap();
        let rows = stmt.query_map([pid], |r| r.get(0)).unwrap();
        rows.collect::<Result<_>>().unwrap()
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
    fn remove_keeps_order_of_survivors() {
        let mut conn = mem();
        let pl = create_playlist(&conn, "p", "playlist", None).unwrap();
        let ids: Vec<i64> = (0..3).map(|i| add_track(&conn, &format!("/r{i}"))).collect();
        add_tracks_to_playlist(&mut conn, pl, &ids).unwrap();
        // Saca el del medio: los ranks de los otros dos no se tocan y el
        // orden relativo se mantiene.
        let ranks_before = ranks_of(&conn, pl);
        remove_tracks_from_playlist(&mut conn, pl, &[ids[1]]).unwrap();
        assert_eq!(order_of(&conn, pl), vec![ids[0], ids[2]]);
        assert_eq!(ranks_of(&conn, pl), vec![ranks_before[0].clone(), ranks_before[2].clone()]);
    }

    /// Reordenar sólo debe reescribir el rank del bloque movido. Es la
    /// propiedad que hace que dos dispositivos reordenando offline mergeen
    /// sin pisarse — si se renumerara todo, cada reorden tocaría cada fila.
    #[test]
    fn reorder_only_rewrites_moved_rows() {
        let mut conn = mem();
        let pl = create_playlist(&conn, "p", "playlist", None).unwrap();
        let ids: Vec<i64> = (0..5).map(|i| add_track(&conn, &format!("/m{i}"))).collect();
        add_tracks_to_playlist(&mut conn, pl, &ids).unwrap();
        let before: std::collections::HashMap<i64, String> =
            order_of(&conn, pl).into_iter().zip(ranks_of(&conn, pl)).collect();

        reorder_playlist_tracks(&mut conn, pl, &[ids[0]], 3).unwrap();

        let after: std::collections::HashMap<i64, String> =
            order_of(&conn, pl).into_iter().zip(ranks_of(&conn, pl)).collect();
        for id in &ids[1..] {
            assert_eq!(before[id], after[id], "el track {id} no se movio, su rank no debe cambiar");
        }
        assert_ne!(before[&ids[0]], after[&ids[0]]);
    }

    #[test]
    fn auto_sync_xml_defaults_true_and_persists() {
        let conn = mem();
        assert!(get_auto_sync_xml(&conn).unwrap());
        set_auto_sync_xml(&conn, false).unwrap();
        assert!(!get_auto_sync_xml(&conn).unwrap());
        set_auto_sync_xml(&conn, true).unwrap();
        assert!(get_auto_sync_xml(&conn).unwrap());
    }

    /// Una base vieja (con `position` INTEGER y sin `rank`) tiene que salir de
    /// la migracion con exactamente el mismo orden que el usuario veia.
    #[test]
    fn migrate_backfills_ranks_preserving_old_order() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE tracks (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE,
                title TEXT NOT NULL DEFAULT '', artist TEXT NOT NULL DEFAULT '',
                album TEXT NOT NULL DEFAULT '', genre TEXT NOT NULL DEFAULT '',
                duration_ms INTEGER NOT NULL DEFAULT 0, bpm INTEGER);
             CREATE TABLE playlists (id INTEGER PRIMARY KEY, name TEXT NOT NULL,
                kind TEXT NOT NULL DEFAULT 'playlist', parent_id INTEGER,
                position INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE playlist_tracks (playlist_id INTEGER NOT NULL, track_id INTEGER NOT NULL,
                position INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (playlist_id, track_id));
             INSERT INTO playlists (id, name, parent_id, position) VALUES
                (1,'b',NULL,1),(2,'a',NULL,0),(3,'c',NULL,2);
             INSERT INTO tracks (id, path) VALUES (10,'/x'),(11,'/y'),(12,'/z');
             INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES
                (1,12,0),(1,10,1),(1,11,2);",
        )
        .unwrap();
        migrate(&conn).unwrap();

        let names: Vec<String> = list_playlists(&conn).unwrap().into_iter().map(|n| n.name).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
        assert_eq!(order_of(&conn, 1), vec![12, 10, 11]);
        // Idempotente: correrla de nuevo no reescribe nada.
        let ranks = ranks_of(&conn, 1);
        migrate(&conn).unwrap();
        assert_eq!(ranks_of(&conn, 1), ranks);
    }

    /// Sin tombstone, el proximo sync ve que el otro dispositivo todavia tiene
    /// la fila y la reenvia: lo borrado reaparece. Por eso todo borrado deja
    /// constancia, incluidos los hijos que se lleva puestos el CASCADE.
    #[test]
    fn deleting_a_folder_leaves_tombstones_for_the_whole_subtree() {
        let conn = mem();
        let f = create_playlist(&conn, "f", "folder", None).unwrap();
        let sub = create_playlist(&conn, "sub", "folder", Some(f)).unwrap();
        let pl = create_playlist(&conn, "p", "playlist", Some(sub)).unwrap();
        let uids: Vec<String> = [f, sub, pl]
            .iter()
            .map(|id| playlist_uid(&conn, *id).unwrap().unwrap())
            .collect();

        delete_playlist(&conn, f).unwrap();

        let mut expected = uids.clone();
        expected.sort();
        assert_eq!(tombstones_of(&conn, "playlist"), expected);
    }

    #[test]
    fn removing_a_track_from_a_playlist_is_recorded_and_reversible() {
        let mut conn = mem();
        let pl = create_playlist(&conn, "p", "playlist", None).unwrap();
        let t = add_track(&conn, "/one");
        let pair = format!(
            "{}:{}",
            playlist_uid(&conn, pl).unwrap().unwrap(),
            track_uid(&conn, t).unwrap().unwrap()
        );
        add_tracks_to_playlist(&mut conn, pl, &[t]).unwrap();

        remove_tracks_from_playlist(&mut conn, pl, &[t]).unwrap();
        assert_eq!(tombstones_of(&conn, "playlist_track"), vec![pair.clone()]);

        // Volver a agregarlo tiene que levantar el tombstone: si no, la union
        // del merge lo vuelve a sacar en el proximo sync.
        add_tracks_to_playlist(&mut conn, pl, &[t]).unwrap();
        assert!(tombstones_of(&conn, "playlist_track").is_empty());
    }

    #[test]
    fn deleting_tracks_records_tombstones() {
        let mut conn = mem();
        let t1 = add_track(&conn, "/gone1");
        let t2 = add_track(&conn, "/gone2");
        let mut uids = vec![
            track_uid(&conn, t1).unwrap().unwrap(),
            track_uid(&conn, t2).unwrap().unwrap(),
        ];
        uids.sort();
        delete_tracks(&mut conn, std::path::Path::new("/nada"), &[t1, t2]).unwrap();
        assert_eq!(tombstones_of(&conn, "track"), uids);
    }

    #[test]
    fn uids_are_assigned_to_legacy_rows_and_are_stable() {
        let conn = mem();
        conn.execute("INSERT INTO tracks (path) VALUES ('/viejo')", []).unwrap();
        conn.execute("INSERT INTO playlists (name, rank) VALUES ('vieja', 'V')", []).unwrap();
        assign_missing_uids(&conn).unwrap();

        let tuid: Option<String> = conn
            .query_row("SELECT uid FROM tracks WHERE path = '/viejo'", [], |r| r.get(0))
            .unwrap();
        assert!(tuid.is_some());
        // Segunda corrida: no reasigna (el uid tiene que ser estable de por
        // vida, es lo que referencian playlists y tombstones).
        assign_missing_uids(&conn).unwrap();
        let again: Option<String> = conn
            .query_row("SELECT uid FROM tracks WHERE path = '/viejo'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tuid, again);
    }

    #[test]
    fn device_identity_is_generated_once() {
        let conn = mem();
        let uid = this_device_uid(&conn).unwrap();
        assert!(!uid.is_empty());
        assert_eq!(this_device_uid(&conn).unwrap(), uid);
        set_device_name(&conn, "PC del living").unwrap();
        assert_eq!(device_name(&conn).unwrap(), "PC del living");
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

