//! Sync selectiva: qué playlists quiere cada dispositivo (Fase 5.7).
//!
//! El scope **es dato replicado**, no configuración local. Se edita desde
//! cualquier dispositivo — desde la PC se decide qué baja el celular — y viaja
//! como todo lo demás, con `updated_at` por fila y el más nuevo gana. La razón
//! es que el scope describe un deseo ("quiero estas playlists en el celu"), no
//! una regla de seguridad. La política de borrados, que sí protege, se queda
//! local y sólo se edita en el dispositivo que protege.
//!
//! Dos cosas que el scope NO hace, a propósito:
//!
//! - **No filtra los datos, filtra la vista.** Playlists, orden y metadata se
//!   replican enteros siempre; lo único selectivo son los archivos de audio.
//!   Filtrar también las filas dejaría al dispositivo sin saber qué existe, y
//!   no habría a qué volver.
//!
//!   Lo que sí cambia es qué se muestra, y depende de si el archivo todavía
//!   está: fuera de scope **con** archivo se ve apagado y no se puede usar
//!   (está de salida); fuera de scope **sin** archivo —ya liberado— desaparece
//!   de la vista principal. El editor de scope, en cambio, muestra todo
//!   siempre: es el único lugar desde donde se puede volver a marcar lo que se
//!   escondió, así que esconderlo ahí sería un callejón sin salida.
//! - **No borra.** Desmarcar corta el sync y nada más: los archivos que ya
//!   están se quedan donde están. Liberar el espacio es una acción aparte y
//!   explícita (`evictable` / `evict`), porque desmarcar suele ser un error de
//!   dedo y nadie quiere que un click se lleve 2 GB en silencio.

use crate::manifest::{Membership, PlaylistEntry, ScopeEntry, DeviceSync};
use rusqlite::{Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Todo lo que haya en la biblioteca.
    All,
    /// Sólo lo que cuelgue de las playlists/carpetas marcadas.
    Selected,
}

impl Mode {
    pub fn from_setting(s: &str) -> Self {
        if s == "selected" {
            Self::Selected
        } else {
            Self::All
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Selected => "selected",
        }
    }
}

/// Qué hace un dispositivo: manda, recibe, las dos, o nada.
///
/// Es una propiedad **del dispositivo**, no del vínculo: la PC manda y recibe,
/// el celular sólo recibe, y eso vale con quien sea. Entre dos, A → B pasa
/// sólo si A manda **y** B recibe. Como es dato replicado, los dos lados leen
/// las mismas dos filas y llegan a la misma conclusión sin negociar nada.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Direction {
    pub sends: bool,
    pub receives: bool,
}

impl Direction {
    pub fn from_setting(s: &str) -> Self {
        match s {
            "off" => Direction { sends: false, receives: false },
            "send" => Direction { sends: true, receives: false },
            "receive" => Direction { sends: false, receives: true },
            _ => Direction { sends: true, receives: true },
        }
    }
}

impl Default for Direction {
    fn default() -> Self {
        Direction { sends: true, receives: true }
    }
}

/// Lo que un dispositivo quiere: dirección, modo y los uids marcados a mano.
#[derive(Debug, Clone)]
pub struct Scope {
    pub mode: Mode,
    pub direction: Direction,
    pub selected: HashSet<String>,
}

impl Default for Scope {
    fn default() -> Self {
        Self {
            mode: Mode::All,
            direction: Direction::default(),
            selected: HashSet::new(),
        }
    }
}

/// Qué se puede mover entre dos dispositivos: `takes` = `local` trae de
/// `remote`, `gives` = `local` le manda a `remote`.
pub fn link(local: &Direction, remote: &Direction) -> (bool, bool) {
    (local.receives && remote.sends, local.sends && remote.receives)
}

/// Igual, resolviendo las dos direcciones desde las filas replicadas de los
/// dos manifests (gana la más nueva de cada dispositivo).
pub fn link_from(
    local_uid: &str,
    remote_uid: &str,
    mine: &[DeviceSync],
    theirs: &[DeviceSync],
) -> (bool, bool) {
    let merged = merge_device_sync(mine, theirs);
    let l = from_entries(local_uid, &[], &merged).direction;
    let r = from_entries(remote_uid, &[], &merged).direction;
    link(&l, &r)
}

// ---------------------------------------------------------------------------
// Resolución (pura)
// ---------------------------------------------------------------------------

/// Marcar una carpeta marca todo lo que cuelga de ella. Si no, marcar "Sets"
/// no traería ninguna de las playlists de adentro y el árbol sería decorativo.
pub fn expand(playlists: &[PlaylistEntry], selected: &HashSet<String>) -> HashSet<String> {
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for p in playlists {
        if let Some(parent) = p.parent_uid.as_deref() {
            children.entry(parent).or_default().push(&p.uid);
        }
    }
    let mut out: HashSet<String> = HashSet::new();
    let mut stack: Vec<&str> = selected.iter().map(|s| s.as_str()).collect();
    while let Some(uid) = stack.pop() {
        if !out.insert(uid.to_string()) {
            continue; // ya visitado — corta también un ciclo si lo hubiera
        }
        if let Some(kids) = children.get(uid) {
            stack.extend(kids.iter().copied());
        }
    }
    out
}

/// Qué tracks entran en el scope. `None` significa "todos": es distinto de un
/// conjunto vacío, que significa "ninguno".
pub fn tracks_in_scope(
    playlists: &[PlaylistEntry],
    memberships: &[Membership],
    scope: &Scope,
) -> Option<HashSet<String>> {
    if scope.mode == Mode::All {
        return None;
    }
    let wanted = expand(playlists, &scope.selected);
    Some(
        memberships
            .iter()
            .filter(|m| wanted.contains(&m.playlist_uid))
            .map(|m| m.track_uid.clone())
            .collect(),
    )
}

/// Reconstruye el scope de un dispositivo a partir de filas replicadas ya
/// mergeadas (las que viajan en el manifest).
pub fn from_entries(device_uid: &str, entries: &[ScopeEntry], modes: &[DeviceSync]) -> Scope {
    let row = modes.iter().find(|m| m.device_uid == device_uid);
    let selected = entries
        .iter()
        .filter(|e| e.device_uid == device_uid && e.selected)
        .map(|e| e.playlist_uid.clone())
        .collect();
    Scope {
        mode: row.map(|m| Mode::from_setting(&m.mode)).unwrap_or(Mode::All),
        direction: row
            .map(|m| Direction::from_setting(&m.direction))
            .unwrap_or_default(),
        selected,
    }
}

/// Une las filas de scope de los dos lados quedándose con la más nueva de cada
/// una. Se usa para planificar: durante la ventana en que un cambio de scope
/// todavía no viajó, los dos manifests difieren y hay que decidir con el más
/// reciente, no con el propio.
pub fn merge_entries(a: &[ScopeEntry], b: &[ScopeEntry]) -> Vec<ScopeEntry> {
    let mut best: HashMap<(String, String), ScopeEntry> = HashMap::new();
    for e in a.iter().chain(b.iter()) {
        let key = (e.device_uid.clone(), e.playlist_uid.clone());
        match best.get(&key) {
            Some(prev) if prev.updated_at >= e.updated_at => {}
            _ => {
                best.insert(key, e.clone());
            }
        }
    }
    best.into_values().collect()
}

pub fn merge_device_sync(a: &[DeviceSync], b: &[DeviceSync]) -> Vec<DeviceSync> {
    let mut best: HashMap<String, DeviceSync> = HashMap::new();
    for m in a.iter().chain(b.iter()) {
        match best.get(&m.device_uid) {
            Some(prev) if prev.updated_at >= m.updated_at => {}
            _ => {
                best.insert(m.device_uid.clone(), m.clone());
            }
        }
    }
    best.into_values().collect()
}

// ---------------------------------------------------------------------------
// Lectura y escritura en la DB
// ---------------------------------------------------------------------------

pub fn entries(conn: &Connection) -> rusqlite::Result<Vec<ScopeEntry>> {
    let mut stmt =
        conn.prepare("SELECT device_uid, playlist_uid, selected, updated_at FROM sync_scope")?;
    let rows = stmt.query_map([], |r| {
        Ok(ScopeEntry {
            device_uid: r.get(0)?,
            playlist_uid: r.get(1)?,
            selected: r.get::<_, i64>(2)? != 0,
            updated_at: r.get(3)?,
        })
    })?;
    rows.collect()
}

pub fn all_device_sync(conn: &Connection) -> rusqlite::Result<Vec<DeviceSync>> {
    let mut stmt =
        conn.prepare("SELECT device_uid, mode, direction, updated_at FROM device_sync")?;
    let rows = stmt.query_map([], |r| {
        Ok(DeviceSync {
            device_uid: r.get(0)?,
            mode: r.get(1)?,
            direction: r.get(2)?,
            updated_at: r.get(3)?,
        })
    })?;
    rows.collect()
}

pub fn get(conn: &Connection, device_uid: &str) -> rusqlite::Result<Scope> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT mode, direction FROM device_sync WHERE device_uid = ?1",
            [device_uid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let mut stmt = conn.prepare(
        "SELECT playlist_uid FROM sync_scope WHERE device_uid = ?1 AND selected = 1",
    )?;
    let rows = stmt.query_map([device_uid], |r| r.get::<_, String>(0))?;
    Ok(Scope {
        mode: Mode::from_setting(row.as_ref().map(|r| r.0.as_str()).unwrap_or("all")),
        direction: Direction::from_setting(row.as_ref().map(|r| r.1.as_str()).unwrap_or("both")),
        selected: rows.collect::<rusqlite::Result<_>>()?,
    })
}

pub fn set_mode(conn: &Connection, device_uid: &str, mode: Mode) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO device_sync (device_uid, mode, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(device_uid) DO UPDATE SET mode = excluded.mode,
                                               updated_at = excluded.updated_at",
        rusqlite::params![device_uid, mode.as_str(), crate::db::now_ms()],
    )?;
    Ok(())
}

/// Qué hace ese dispositivo. Se edita desde cualquier lado, igual que el
/// scope: es una preferencia, no una defensa.
pub fn set_direction(conn: &Connection, device_uid: &str, direction: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO device_sync (device_uid, direction, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(device_uid) DO UPDATE SET direction = excluded.direction,
                                               updated_at = excluded.updated_at",
        rusqlite::params![device_uid, direction, crate::db::now_ms()],
    )?;
    Ok(())
}

/// Marca o desmarca una playlist. Desmarcar deja la fila con `selected = 0`:
/// borrarla haría que la unión del merge la trajera de vuelta del otro lado.
pub fn set_playlist(
    conn: &Connection,
    device_uid: &str,
    playlist_uid: &str,
    selected: bool,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO sync_scope (device_uid, playlist_uid, selected, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(device_uid, playlist_uid) DO UPDATE SET
            selected = excluded.selected, updated_at = excluded.updated_at",
        rusqlite::params![
            device_uid,
            playlist_uid,
            i64::from(selected),
            crate::db::now_ms()
        ],
    )?;
    Ok(())
}

/// Una playlist creada por el usuario en este dispositivo arranca marcada.
///
/// Sin esto, en modo selectivo, crearla la haría desaparecer del árbol en el
/// acto: nace sin fila de scope —o sea desmarcada— y sin archivos, que es
/// exactamente la combinación que se esconde. Vale sólo para las que se crean
/// acá: las que llegan por sync siguen sin marcar, que es lo que hace que la
/// sync sea selectiva.
pub fn select_new_local(conn: &Connection, playlist_id: i64) -> rusqlite::Result<()> {
    let me = crate::db::this_device_uid(conn)?;
    if get(conn, &me)?.mode == Mode::All {
        return Ok(());
    }
    let uid: Option<String> = conn
        .query_row("SELECT uid FROM playlists WHERE id = ?1", [playlist_id], |r| {
            r.get(0)
        })
        .optional()?;
    match uid {
        Some(uid) => set_playlist(conn, &me, &uid, true),
        None => Ok(()),
    }
}

/// Aplica una fila que llegó del otro lado. Devuelve `true` si cambió algo.
pub fn apply_entry(conn: &Connection, e: &ScopeEntry) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "INSERT INTO sync_scope (device_uid, playlist_uid, selected, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(device_uid, playlist_uid) DO UPDATE SET
            selected = excluded.selected, updated_at = excluded.updated_at
          WHERE excluded.updated_at > sync_scope.updated_at",
        rusqlite::params![
            e.device_uid,
            e.playlist_uid,
            i64::from(e.selected),
            e.updated_at
        ],
    )?;
    Ok(n > 0)
}

/// LWW sobre la fila entera: modo y dirección viajan juntos. Un cambio de
/// dirección hecho en un dispositivo puede pisar un cambio de modo hecho en el
/// otro al mismo tiempo — es la misma regla que el resto de la fase, y la
/// alternativa (un reloj por campo) no vale la pena para dos ajustes que
/// cambian una vez cada tanto.
pub fn apply_device_sync(conn: &Connection, m: &DeviceSync) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "INSERT INTO device_sync (device_uid, mode, direction, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(device_uid) DO UPDATE SET mode = excluded.mode,
                                               direction = excluded.direction,
                                               updated_at = excluded.updated_at
          WHERE excluded.updated_at > device_sync.updated_at",
        rusqlite::params![m.device_uid, m.mode, m.direction, m.updated_at],
    )?;
    Ok(n > 0)
}

// ---------------------------------------------------------------------------
// Réplicas conocidas de cada blob
// ---------------------------------------------------------------------------

/// Anota que `device_uid` tiene esos archivos. Es lo único que hace seguro
/// liberar espacio: sin una copia confirmada en otro lado, evacuar un archivo
/// podría ser destruir el último ejemplar.
/// Va todo en UNA transacción y con el statement preparado una sola vez: son
/// tantas filas como tracks tenga el otro dispositivo, y en autocommit cada
/// INSERT es un fsync. Con una biblioteca de mil temas eso son mil fsyncs por
/// sync, con el lock de la DB tomado — en el celular se siente como que la app
/// se colgó.
pub fn note_replicas(conn: &Connection, device_uid: &str, hashes: &[String]) -> rusqlite::Result<()> {
    if hashes.is_empty() {
        return Ok(());
    }
    let now = crate::db::now_ms();
    conn.execute_batch("BEGIN")?;
    let result = (|| -> rusqlite::Result<()> {
        // El `WHERE` no es cosmético: sin él, cada sync reescribe una fila por
        // track del otro dispositivo aunque no haya cambiado nada, ensuciando
        // páginas y haciendo trabajar al WAL para nada. Refrescar la fecha una
        // vez por hora alcanza — esto sólo responde "¿alguien más lo tiene?".
        let mut stmt = conn.prepare_cached(
            "INSERT INTO blob_replicas (hash, device_uid, seen_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(hash, device_uid) DO UPDATE SET seen_at = excluded.seen_at
              WHERE excluded.seen_at - blob_replicas.seen_at > 3600000",
        )?;
        for h in hashes {
            stmt.execute(rusqlite::params![h, device_uid, now])?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => conn.execute_batch("COMMIT"),
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Qué tracks (por uid) están en el scope de un dispositivo, leído de la DB.
/// `None` = todos.
///
/// Resuelto en SQL —con un CTE recursivo para bajar por el árbol— y no
/// cargando la tabla de membresías en memoria: esto lo llama `list_tracks` en
/// cada refresco de la biblioteca, y traerse miles de filas para descartarlas
/// se nota en el celular. La regla es la misma que la de `tracks_in_scope`
/// (hay un test que exige que coincidan).
pub fn scope_tracks(conn: &Connection, device_uid: &str) -> rusqlite::Result<Option<HashSet<String>>> {
    let scope = get(conn, device_uid)?;
    if scope.mode == Mode::All {
        return Ok(None);
    }
    let mut stmt = conn.prepare_cached(
        "WITH RECURSIVE marcadas(uid) AS (
             SELECT playlist_uid FROM sync_scope
              WHERE device_uid = ?1 AND selected = 1
             UNION
             SELECT hija.uid FROM playlists hija
               JOIN playlists madre ON madre.id = hija.parent_id
               JOIN marcadas ON marcadas.uid = madre.uid
             WHERE hija.uid IS NOT NULL
         )
         SELECT DISTINCT t.uid
           FROM playlist_tracks pt
           JOIN playlists p ON p.id = pt.playlist_id
           JOIN tracks t ON t.id = pt.track_id
          WHERE t.uid IS NOT NULL AND p.uid IN (SELECT uid FROM marcadas)",
    )?;
    let rows = stmt.query_map([device_uid], |r| r.get::<_, String>(0))?;
    Ok(Some(rows.collect::<rusqlite::Result<HashSet<_>>>()?))
}

/// Igual que `scope_tracks`, pero mirando sólo los tracks de UNA playlist.
///
/// Abrir una playlist llamaba a `scope_tracks`, que arma el set en-scope de la
/// biblioteca ENTERA —recorriendo todas las membresías, con un DISTINCT sobre
/// uids— para después marcar veinte filas. Con el lock de la DB tomado. Acá se
/// arranca por las filas de esa playlist, que son pocas, y se sale por
/// `playlist_tracks(track_id)` a ver si alguna de las otras playlists del tema
/// está marcada.
pub fn scope_tracks_of_playlist(
    conn: &Connection,
    device_uid: &str,
    playlist_id: i64,
) -> rusqlite::Result<Option<HashSet<String>>> {
    if get(conn, device_uid)?.mode == Mode::All {
        return Ok(None);
    }
    let mut stmt = conn.prepare_cached(
        "WITH RECURSIVE marcadas(uid) AS (
             SELECT playlist_uid FROM sync_scope
              WHERE device_uid = ?1 AND selected = 1
             UNION
             SELECT hija.uid FROM playlists hija
               JOIN playlists madre ON madre.id = hija.parent_id
               JOIN marcadas ON marcadas.uid = madre.uid
             WHERE hija.uid IS NOT NULL
         )
         SELECT DISTINCT t.uid
           FROM playlist_tracks aca
           JOIN playlist_tracks otras ON otras.track_id = aca.track_id
           JOIN playlists p ON p.id = otras.playlist_id
           JOIN tracks t ON t.id = aca.track_id
          WHERE aca.playlist_id = ?2
            AND t.uid IS NOT NULL
            AND p.uid IN (SELECT uid FROM marcadas)",
    )?;
    let rows = stmt.query_map(rusqlite::params![device_uid, playlist_id], |r| {
        r.get::<_, String>(0)
    })?;
    Ok(Some(rows.collect::<rusqlite::Result<HashSet<_>>>()?))
}

/// Qué playlists (por uid) están marcadas para un dispositivo, ya expandidas
/// hacia abajo por el árbol. `None` = todas.
///
/// Ojo con las carpetas: una carpeta que no está marcada pero que contiene una
/// playlist marcada NO sale de acá, y sin embargo tiene que verse — si no, la
/// marcada de adentro queda huérfana y sin forma de llegar a ella. Esa parte la
/// resuelve el árbol del frontend, que muestra a cualquier nodo con un
/// descendiente visible.
pub fn scope_playlists(
    conn: &Connection,
    device_uid: &str,
) -> rusqlite::Result<Option<HashSet<String>>> {
    if get(conn, device_uid)?.mode == Mode::All {
        return Ok(None);
    }
    let mut stmt = conn.prepare_cached(
        "WITH RECURSIVE marcadas(uid) AS (
             SELECT playlist_uid FROM sync_scope
              WHERE device_uid = ?1 AND selected = 1
             UNION
             SELECT hija.uid FROM playlists hija
               JOIN playlists madre ON madre.id = hija.parent_id
               JOIN marcadas ON marcadas.uid = madre.uid
             WHERE hija.uid IS NOT NULL
         )
         SELECT uid FROM marcadas",
    )?;
    let rows = stmt.query_map([device_uid], |r| r.get::<_, String>(0))?;
    Ok(Some(rows.collect::<rusqlite::Result<HashSet<_>>>()?))
}

/// Por playlist, cuántos de sus tracks siguen ocupando lugar **por ella**:
/// tienen archivo acá y no entran en el scope de este dispositivo. Vacío si el
/// scope es "todo".
///
/// No alcanza con contar los que tienen archivo. Un tema que está en una
/// playlist marcada y en otra desmarcada tiene archivo acá por culpa de la
/// marcada: la desmarcada no lo sostiene y no lo va a soltar nunca, así que
/// contarlo la dejaba visible para siempre. Los que cuentan son los que se van
/// a ir cuando se libere el espacio.
///
/// Es sólo para decidir si la playlist sigue en el árbol. Qué se muestra
/// ADENTRO es otra cosa: ahí se ve todo lo que tenga archivo, prestado o no,
/// porque esconder algo que sigue ocupando lugar es mentir sobre el espacio.
pub fn stranded_counts(
    conn: &Connection,
    device_uid: &str,
) -> rusqlite::Result<HashMap<i64, i64>> {
    if get(conn, device_uid)?.mode == Mode::All {
        return Ok(HashMap::new());
    }
    let mut stmt = conn.prepare_cached(
        "WITH RECURSIVE marcadas(uid) AS (
             SELECT playlist_uid FROM sync_scope
              WHERE device_uid = ?1 AND selected = 1
             UNION
             SELECT hija.uid FROM playlists hija
               JOIN playlists madre ON madre.id = hija.parent_id
               JOIN marcadas ON marcadas.uid = madre.uid
             WHERE hija.uid IS NOT NULL
         ),
         sostenidos(track_id) AS (
             SELECT DISTINCT pt.track_id
               FROM playlist_tracks pt
               JOIN playlists p ON p.id = pt.playlist_id
              WHERE p.uid IN (SELECT uid FROM marcadas)
         )
         SELECT pt.playlist_id, COUNT(*)
           FROM playlist_tracks pt
           JOIN tracks t ON t.id = pt.track_id
          WHERE t.local_state = 'present'
            AND pt.track_id NOT IN (SELECT track_id FROM sostenidos)
          GROUP BY pt.playlist_id",
    )?;
    let rows = stmt.query_map([device_uid], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// Liberar espacio
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Evictable {
    pub id: i64,
    pub path: String,
    pub size: i64,
}

/// Archivos que este dispositivo podría soltar: están acá, quedaron fuera del
/// scope, y **consta que viven en otro dispositivo vinculado**.
///
/// Ese último requisito es el que hace que "Liberar espacio" no pueda perder
/// música. Un track fuera de scope que no está en ningún otro lado no se
/// ofrece: la única copia se queda donde está.
pub fn evictable(
    conn: &Connection,
    music_dir: &std::path::Path,
) -> rusqlite::Result<Vec<Evictable>> {
    let me = crate::db::this_device_uid(conn)?;
    if get(conn, &me)?.mode == Mode::All {
        return Ok(Vec::new()); // scope = todo: no hay nada de más
    }

    // Todo el descarte en UNA consulta, y el anti-join por `track_id` (entero)
    // en vez de por `uid` (texto).
    //
    // Antes esto traía a Rust CADA track presente de la biblioteca para
    // descartarlos en un `for`, y aparte armaba el set en-scope entero con un
    // DISTINCT sobre uids. Con el lock de la DB tomado, que es global: mientras
    // corría, cualquier otra cosa que tocara la DB —abrir una playlist,
    // arrancar un tema— quedaba encolada. Es lo que hacía que abrir el panel de
    // sync trabara la app entera.
    let mut stmt = conn.prepare_cached(
        "WITH RECURSIVE marcadas(uid) AS (
             SELECT playlist_uid FROM sync_scope
              WHERE device_uid = ?1 AND selected = 1
             UNION
             SELECT hija.uid FROM playlists hija
               JOIN playlists madre ON madre.id = hija.parent_id
               JOIN marcadas ON marcadas.uid = madre.uid
             WHERE hija.uid IS NOT NULL
         ),
         en_scope(track_id) AS (
             SELECT DISTINCT pt.track_id
               FROM playlist_tracks pt
               JOIN playlists p ON p.id = pt.playlist_id
              WHERE p.uid IN (SELECT uid FROM marcadas)
         )
         SELECT t.id, t.path, COALESCE(t.size_bytes, 0)
           FROM tracks t
          WHERE t.local_state = 'present' AND t.uid IS NOT NULL
            AND t.content_hash IS NOT NULL
            AND t.id NOT IN (SELECT track_id FROM en_scope)
            AND EXISTS (SELECT 1 FROM blob_replicas r
                         WHERE r.hash = t.content_hash AND r.device_uid <> ?1)",
    )?;
    let rows = stmt.query_map([&me], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, path, size) = row?;
        // Un archivo legacy de afuera de la carpeta gestionada no es nuestro
        // para moverlo. Sobre las pocas filas que sobrevivieron, no sobre todas.
        if !std::path::Path::new(&path).starts_with(music_dir) {
            continue;
        }
        out.push(Evictable { id, path, size });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Rescate desde la papelera
// ---------------------------------------------------------------------------
//
// Re-marcar una playlist liberada no puede volver a bajar por la red archivos
// que siguen en `.sway-trash`, a un `rename` de distancia. Pero la
// verificación es por hash, y hashear es caro: el trabajo va partido en tres
// —buscar candidatos, hashear, aplicar— porque **el hash NO puede calcularse
// con el lock de la DB tomado**. Sostenerlo mientras se hashean gigabytes
// congela toda la UI: cada `list_tracks` del frontend se queda esperando el
// mutex. Es exactamente el mismo motivo por el que el backfill de hashes
// suelta el lock entre archivo y archivo.

/// Una fila sin archivo que habría que recuperar.
#[derive(Debug, Clone)]
pub struct Restorable {
    pub id: i64,
    pub hash: String,
    pub rel_path: String,
    pub size: i64,
}

/// Qué falta y podría estar en la papelera. Sólo consulta la DB — barato.
pub fn restorable(conn: &Connection) -> rusqlite::Result<Vec<Restorable>> {
    let me = crate::db::this_device_uid(conn)?;
    let selectivo = get(conn, &me)?.mode == Mode::Selected;
    // El filtro de scope va en SQL, igual que en `evictable`: esto corre en cada
    // sync y en cada cambio de scope, con el lock global tomado.
    let sql = if selectivo {
        "WITH RECURSIVE marcadas(uid) AS (
             SELECT playlist_uid FROM sync_scope
              WHERE device_uid = ?1 AND selected = 1
             UNION
             SELECT hija.uid FROM playlists hija
               JOIN playlists madre ON madre.id = hija.parent_id
               JOIN marcadas ON marcadas.uid = madre.uid
             WHERE hija.uid IS NOT NULL
         ),
         en_scope(track_id) AS (
             SELECT DISTINCT pt.track_id
               FROM playlist_tracks pt
               JOIN playlists p ON p.id = pt.playlist_id
              WHERE p.uid IN (SELECT uid FROM marcadas)
         )
         SELECT id, content_hash, rel_path, COALESCE(size_bytes, 0)
           FROM tracks
          WHERE local_state <> 'present' AND uid IS NOT NULL
            AND content_hash IS NOT NULL AND COALESCE(rel_path, '') <> ''
            AND id IN (SELECT track_id FROM en_scope)"
    } else {
        "SELECT id, content_hash, rel_path, COALESCE(size_bytes, 0)
           FROM tracks
          WHERE local_state <> 'present' AND uid IS NOT NULL
            AND content_hash IS NOT NULL AND COALESCE(rel_path, '') <> ''
            AND ?1 IS NOT NULL"
    };
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map([&me], |r| {
        Ok(Restorable {
            id: r.get(0)?,
            hash: r.get(1)?,
            rel_path: r.get(2)?,
            size: r.get(3)?,
        })
    })?;
    rows.collect()
}

/// Busca esos archivos en la papelera. **Hashea: correr sin el lock.**
///
/// El índice de la papelera se arma UNA vez, agrupado por tamaño. Antes se
/// releía el directorio por candidato, así que N faltantes contra M archivos
/// en la papelera daban N×M hashes — con la papelera llena, minutos de CPU en
/// cada sync.
pub fn find_in_trash(
    music_dir: &std::path::Path,
    candidates: &[Restorable],
) -> Vec<(Restorable, std::path::PathBuf)> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let trash = crate::trash::trash_dir(music_dir);
    let Ok(entries) = std::fs::read_dir(&trash) else {
        return Vec::new();
    };
    // tamaño -> archivos de la papelera con ese tamaño.
    let mut by_size: HashMap<u64, Vec<std::path::PathBuf>> = HashMap::new();
    for e in entries.flatten() {
        let Ok(md) = e.metadata() else { continue };
        if md.is_file() {
            by_size.entry(md.len()).or_default().push(e.path());
        }
    }
    if by_size.is_empty() {
        return Vec::new();
    }

    // El hash de cada archivo de la papelera se calcula a lo sumo una vez.
    let mut hashed: HashMap<std::path::PathBuf, String> = HashMap::new();
    let mut out = Vec::new();
    for c in candidates {
        let Some(same_size) = by_size.get(&(c.size as u64)) else {
            continue; // sin candidato del mismo tamaño no hay nada que hashear
        };
        // El nombre primero: la papelera lo conserva salvo que hubiera chocado.
        let preferred = trash.join(&c.rel_path);
        let order = same_size
            .iter()
            .filter(|p| **p == preferred)
            .chain(same_size.iter().filter(|p| **p != preferred));
        for p in order {
            let h = match hashed.get(p) {
                Some(h) => h.clone(),
                None => {
                    let Ok(h) = crate::hashing::hash_file(p) else { continue };
                    hashed.insert(p.clone(), h.clone());
                    h
                }
            };
            if h == c.hash {
                out.push((c.clone(), p.clone()));
                break;
            }
        }
    }
    out
}

/// Mueve lo encontrado de vuelta a la biblioteca y actualiza las filas.
/// Barato: renames y updates, nada de hashear.
///
/// `expect` marca el destino antes de escribirlo: el watcher observa la
/// carpeta gestionada y, sin eso, auto-importaría el archivo recuperado como
/// si fuera nuevo — fila nueva, uid nuevo, identidad compartida rota.
pub fn finish_restore(
    conn: &Connection,
    music_dir: &std::path::Path,
    found: &[(Restorable, std::path::PathBuf)],
    expect: &dyn Fn(&std::path::Path),
) -> rusqlite::Result<usize> {
    let mut restored = 0;
    for (c, src) in found {
        // Si en la carpeta gestionada ya hay un archivo con ese nombre y
        // tamaño, `managed_dest_for` devuelve ese mismo: reapareció por otro
        // camino (una copia a mano, un sync que llegó primero) y sólo falta
        // reapuntar la fila. Si no, se mueve el de la papelera.
        let dest = crate::import::managed_dest_for(music_dir, &c.rel_path, c.size as u64);
        expect(&dest);
        if !dest.exists() && std::fs::rename(src, &dest).is_err() {
            continue;
        }
        let (_, mtime) = crate::hashing::file_stamp(&dest).unwrap_or((c.size, 0));
        conn.execute(
            "UPDATE tracks SET path = ?1, rel_path = ?2, mtime_ms = ?3,
                    local_state = 'present' WHERE id = ?4",
            rusqlite::params![
                dest.to_string_lossy(),
                dest.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| c.rel_path.clone()),
                mtime,
                c.id
            ],
        )?;
        restored += 1;
    }
    if restored > 0 {
        log::info!("[scope] {restored} archivo(s) recuperados de la papelera");
    }
    Ok(restored)
}

/// Ejecuta la liberación: los archivos van a la papelera de la biblioteca (30
/// días) y las filas quedan en `absent`. La fila NO se borra ni se tombstonea:
/// el track se sigue viendo, en gris, y re-marcar su playlist lo baja de nuevo.
pub fn evict(
    conn: &Connection,
    music_dir: &std::path::Path,
    items: &[Evictable],
) -> rusqlite::Result<(usize, i64)> {
    let (mut n, mut bytes) = (0usize, 0i64);
    for it in items {
        let path = std::path::Path::new(&it.path);
        if path.exists() {
            if let Err(e) = crate::trash::move_to_trash(music_dir, path) {
                log::warn!("[scope] no se pudo evacuar {}: {e}", path.display());
                continue;
            }
        }
        conn.execute(
            "UPDATE tracks SET local_state = 'absent' WHERE id = ?1",
            [it.id],
        )?;
        n += 1;
        bytes += it.size;
    }
    if n > 0 {
        log::info!("[scope] {n} archivo(s) evacuados, {bytes} bytes liberados");
    }
    Ok((n, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn pl(uid: &str, parent: Option<&str>) -> PlaylistEntry {
        PlaylistEntry {
            uid: uid.into(),
            name: uid.into(),
            kind: "playlist".into(),
            parent_uid: parent.map(String::from),
            rank: "V".into(),
            updated_at: 0,
        }
    }

    fn member(playlist: &str, track: &str) -> Membership {
        Membership {
            playlist_uid: playlist.into(),
            track_uid: track.into(),
            rank: "V".into(),
            added_at: 0,
        }
    }

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// Marcar una carpeta tiene que traer lo que cuelga de ella, o el árbol
    /// sería decorativo.
    #[test]
    fn selecting_a_folder_selects_its_subtree() {
        let tree = vec![
            pl("carpeta", None),
            pl("adentro", Some("carpeta")),
            pl("mas-adentro", Some("adentro")),
            pl("afuera", None),
        ];
        let got = expand(&tree, &set(&["carpeta"]));
        assert!(got.contains("adentro") && got.contains("mas-adentro"));
        assert!(!got.contains("afuera"));
    }

    /// La dirección es del dispositivo, no del vínculo: lo que hace la PC vale
    /// con quien sea, y entre dos, algo se mueve sólo si uno manda y el otro
    /// recibe. Los dos lados leen las mismas dos filas y llegan a la misma
    /// conclusión sin negociar nada por la red.
    #[test]
    fn a_device_that_only_receives_never_sends_to_anyone() {
        let pc = Direction::from_setting("both");
        let celu = Direction::from_setting("receive");

        // Visto desde la PC: le manda al celu, no le trae nada.
        assert_eq!(link(&pc, &celu), (false, true));
        // Visto desde el celu: la conclusión es la misma, al revés.
        assert_eq!(link(&celu, &pc), (true, false));
        // Dos que sólo reciben no mueven nada.
        assert_eq!(link(&celu, &celu), (false, false));
        // Y uno en pausa corta con todos.
        assert_eq!(link(&pc, &Direction::from_setting("off")), (false, false));
    }

    #[test]
    fn mode_all_means_every_track_no_matter_the_selection() {
        let scope = Scope { mode: Mode::All, direction: Direction::default(), selected: set(&["x"]) };
        assert!(tracks_in_scope(&[], &[], &scope).is_none());
    }

    #[test]
    fn only_tracks_under_selected_playlists_are_in_scope() {
        let tree = vec![pl("si", None), pl("no", None)];
        let members = vec![member("si", "t1"), member("no", "t2")];
        let scope = Scope { mode: Mode::Selected, direction: Direction::default(), selected: set(&["si"]) };
        let got = tracks_in_scope(&tree, &members, &scope).unwrap();
        assert!(got.contains("t1"));
        assert!(!got.contains("t2"));
    }

    /// Desmarcar del otro lado tiene que pegar: gana el más nuevo, no el local.
    #[test]
    fn the_newest_scope_row_wins() {
        let mine = vec![ScopeEntry {
            device_uid: "celu".into(),
            playlist_uid: "sets".into(),
            selected: true,
            updated_at: 100,
        }];
        let theirs = vec![ScopeEntry {
            device_uid: "celu".into(),
            playlist_uid: "sets".into(),
            selected: false,
            updated_at: 500,
        }];
        let merged = merge_entries(&mine, &theirs);
        assert_eq!(merged.len(), 1);
        assert!(!merged[0].selected);
    }

    fn mem() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        db::init_schema(&conn).unwrap();
        conn
    }

    /// Desmarcar deja la fila con `selected = 0`. Si la borrara, la unión del
    /// merge la traería de vuelta y desmarcar no se pegaría nunca.
    #[test]
    fn unselecting_keeps_a_row_that_says_no() {
        let conn = mem();
        set_playlist(&conn, "celu", "sets", true).unwrap();
        set_playlist(&conn, "celu", "sets", false).unwrap();
        let rows = entries(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].selected);
        assert!(!get(&conn, "celu").unwrap().selected.contains("sets"));
    }

    #[test]
    fn an_older_incoming_row_does_not_win() {
        let conn = mem();
        set_playlist(&conn, "celu", "sets", true).unwrap();
        let now = entries(&conn).unwrap()[0].updated_at;
        let applied = apply_entry(
            &conn,
            &ScopeEntry {
                device_uid: "celu".into(),
                playlist_uid: "sets".into(),
                selected: false,
                updated_at: now - 1000,
            },
        )
        .unwrap();
        assert!(!applied);
        assert!(entries(&conn).unwrap()[0].selected);
    }

    /// Ciclo completo de la sync selectiva: liberar y volver a marcar. El
    /// archivo tiene que salir de la papelera, no de la red — está a un
    /// `rename` de distancia y bajarlo de nuevo sería tirar la conexión (y la
    /// batería del celular) por la ventana.
    #[test]
    fn re_selecting_a_playlist_recovers_the_file_from_the_trash() {
        let conn = mem();
        let me = db::this_device_uid(&conn).unwrap();
        let music = std::env::temp_dir().join(format!(
            "sway-scope-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&music).unwrap();
        let file = music.join("tema.flac");
        std::fs::write(&file, b"audio real").unwrap();
        let hash = crate::hashing::hash_file(&file).unwrap();

        conn.execute(
            "INSERT INTO tracks (path, uid, content_hash, rel_path, size_bytes, local_state)
             VALUES (?1, 'tr', ?2, 'tema.flac', 10, 'present')",
            rusqlite::params![file.to_string_lossy(), hash],
        )
        .unwrap();
        let pl = db::create_playlist(&conn, "Sets", "playlist", None).unwrap();
        let pl_uid: String = conn
            .query_row("SELECT uid FROM playlists WHERE id = ?1", [pl], |r| r.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO playlist_tracks (playlist_id, track_id, rank, added_at)
             VALUES (?1, (SELECT id FROM tracks WHERE uid = 'tr'), 'V', 1)",
            [pl],
        )
        .unwrap();
        note_replicas(&conn, "otro", &[hash.clone()]).unwrap();

        // Scope selectivo con nada marcado: el archivo sobra y se libera.
        set_mode(&conn, &me, Mode::Selected).unwrap();
        let items = evictable(&conn, &music).unwrap();
        assert_eq!(items.len(), 1);
        evict(&conn, &music, &items).unwrap();
        assert!(!file.exists(), "salió de la biblioteca");

        // Se vuelve a marcar la playlist: vuelve de la papelera.
        set_playlist(&conn, &me, &pl_uid, true).unwrap();
        let candidates = restorable(&conn).unwrap();
        assert_eq!(candidates.len(), 1);
        let found = find_in_trash(&music, &candidates);
        assert_eq!(found.len(), 1);
        assert_eq!(finish_restore(&conn, &music, &found, &|_| {}).unwrap(), 1);

        let (state, path): (String, String) = conn
            .query_row("SELECT local_state, path FROM tracks WHERE uid = 'tr'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(state, "present");
        assert!(std::path::Path::new(&path).exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"audio real");
        std::fs::remove_dir_all(&music).ok();
    }

    /// Hay dos implementaciones de la misma regla: la pura (`tracks_in_scope`,
    /// la que decide qué se transfiere) y la de SQL (`scope_tracks`, la que
    /// pinta la biblioteca). Si opinaran distinto, la tabla mostraría una cosa
    /// y el sync haría otra.
    #[test]
    fn the_sql_and_the_pure_rule_agree() {
        let conn = mem();
        let me = db::this_device_uid(&conn).unwrap();
        // Carpeta > sub > playlist, con un track adentro y otro afuera.
        let carpeta = db::create_playlist(&conn, "Carpeta", "folder", None).unwrap();
        let sub = db::create_playlist(&conn, "Sub", "playlist", Some(carpeta)).unwrap();
        let afuera = db::create_playlist(&conn, "Afuera", "playlist", None).unwrap();
        for (i, pl) in [(1, sub), (2, afuera)] {
            conn.execute(
                "INSERT INTO tracks (path, uid, local_state) VALUES (?1, ?2, 'present')",
                rusqlite::params![format!("/m/{i}"), format!("t{i}")],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO playlist_tracks (playlist_id, track_id, rank, added_at)
                 VALUES (?1, (SELECT id FROM tracks WHERE uid = ?2), 'V', 1)",
                rusqlite::params![pl, format!("t{i}")],
            )
            .unwrap();
        }
        let carpeta_uid: String = conn
            .query_row("SELECT uid FROM playlists WHERE id = ?1", [carpeta], |r| r.get(0))
            .unwrap();

        set_mode(&conn, &me, Mode::Selected).unwrap();
        set_playlist(&conn, &me, &carpeta_uid, true).unwrap();

        // Regla de SQL: marcar la carpeta trae el track de la playlist de adentro.
        let via_sql = scope_tracks(&conn, &me).unwrap().unwrap();
        assert!(via_sql.contains("t1"));
        assert!(!via_sql.contains("t2"));

        // Regla pura, sobre los mismos datos: tiene que dar lo mismo.
        let m = crate::manifest::build(&conn).unwrap();
        let via_pura =
            tracks_in_scope(&m.playlists, &m.memberships, &get(&conn, &me).unwrap()).unwrap();
        assert_eq!(via_sql, via_pura);
    }

    /// Lo que decide qué se ve en el árbol de la vista principal. Incluye el
    /// caso de la carpeta a medio marcar, que es el que puede dejar una
    /// playlist visible pero inalcanzable si el frontend no hace su parte.
    #[test]
    fn selected_playlists_expand_down_but_not_up() {
        let conn = mem();
        let me = db::this_device_uid(&conn).unwrap();
        let carpeta = db::create_playlist(&conn, "Carpeta", "folder", None).unwrap();
        let adentro = db::create_playlist(&conn, "Adentro", "playlist", Some(carpeta)).unwrap();
        let afuera = db::create_playlist(&conn, "Afuera", "playlist", None).unwrap();
        let uid = |id: i64| -> String {
            conn.query_row("SELECT uid FROM playlists WHERE id = ?1", [id], |r| r.get(0))
                .unwrap()
        };

        // Sin modo selectivo no se filtra nada.
        assert!(scope_playlists(&conn, &me).unwrap().is_none());

        set_mode(&conn, &me, Mode::Selected).unwrap();
        set_playlist(&conn, &me, &uid(adentro), true).unwrap();
        let got = scope_playlists(&conn, &me).unwrap().unwrap();

        assert!(got.contains(&uid(adentro)));
        assert!(!got.contains(&uid(afuera)));
        // La carpeta que la contiene NO entra: marcar una hija no marca a la
        // madre. Que igual se vea en el árbol es cosa del frontend, que muestra
        // a cualquier nodo con un descendiente visible.
        assert!(!got.contains(&uid(carpeta)));

        // Y al revés sí baja: marcar la carpeta trae lo que cuelga.
        set_playlist(&conn, &me, &uid(carpeta), true).unwrap();
        let got = scope_playlists(&conn, &me).unwrap().unwrap();
        assert!(got.contains(&uid(carpeta)) && got.contains(&uid(adentro)));
        assert!(!got.contains(&uid(afuera)));
    }

    /// El árbol necesita separar "desmarcada pero todavía ocupa lugar" de
    /// "desmarcada y ya liberada": la primera se muestra apagada, la segunda
    /// desaparece. Y el tema que está en las dos no cuenta para la desmarcada:
    /// su archivo lo sostiene la marcada, así que la desmarcada no lo va a
    /// soltar nunca y quedaría visible para siempre mostrando algo prestado.
    #[test]
    fn a_shared_track_does_not_keep_the_unselected_playlist_alive() {
        let conn = mem();
        let me = db::this_device_uid(&conn).unwrap();
        let marcada = db::create_playlist(&conn, "Marcada", "playlist", None).unwrap();
        let no = db::create_playlist(&conn, "Desmarcada", "playlist", None).unwrap();
        let uid_de = |id: i64| -> String {
            conn.query_row("SELECT uid FROM playlists WHERE id = ?1", [id], |r| r.get(0))
                .unwrap()
        };
        let add = |pl: i64, track: &str, state: &str| {
            conn.execute(
                "INSERT OR IGNORE INTO tracks (path, uid, local_state) VALUES (?1, ?2, ?3)",
                rusqlite::params![format!("/m/{track}"), track, state],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO playlist_tracks (playlist_id, track_id, rank, added_at)
                 VALUES (?1, (SELECT id FROM tracks WHERE uid = ?2), 'V', 1)",
                rusqlite::params![pl, track],
            )
            .unwrap();
        };
        // `compartido` está en las dos. `propio` sólo en la desmarcada.
        add(marcada, "compartido", "present");
        add(no, "compartido", "present");
        add(no, "propio", "present");

        set_mode(&conn, &me, Mode::Selected).unwrap();
        set_playlist(&conn, &me, &uid_de(marcada), true).unwrap();

        let c = stranded_counts(&conn, &me).unwrap();
        assert_eq!(c.get(&marcada), None, "lo que está en scope no cuenta");
        assert_eq!(
            c.get(&no).copied().unwrap_or(0),
            1,
            "sólo `propio`: `compartido` lo sostiene la marcada"
        );

        // Pero los dos siguen teniendo archivo, así que al abrir la desmarcada
        // se ven los dos, apagados: mientras ocupen lugar, esconderlos de una
        // lista donde figuran sería mentir sobre el espacio.
        let node = db::list_playlists(&conn)
            .unwrap()
            .into_iter()
            .find(|n| n.id == no)
            .unwrap();
        assert_eq!(node.track_count, 2);
        assert_eq!(node.present_count, 2);

        // Y `compartido` está en scope: es lo que hace que no se apague ni se
        // esconda en la playlist marcada, donde sí participa.
        let en_scope = scope_tracks(&conn, &me).unwrap().unwrap();
        assert!(en_scope.contains("compartido"));
        assert!(!en_scope.contains("propio"));

        // Se libera el espacio: `propio` se va y la desmarcada deja de sostener
        // nada, aunque `compartido` siga acá por la otra.
        conn.execute(
            "UPDATE tracks SET local_state = 'absent' WHERE uid = 'propio'",
            [],
        )
        .unwrap();
        assert_eq!(stranded_counts(&conn, &me).unwrap().get(&no), None);

        // Y con el scope en "todo" no hay nada varado en ningún lado.
        set_mode(&conn, &me, Mode::All).unwrap();
        assert!(stranded_counts(&conn, &me).unwrap().is_empty());
    }

    /// Crear una playlist en modo selectivo no puede hacerla desaparecer: nace
    /// sin fila de scope y sin archivos, que es justo lo que el árbol esconde.
    #[test]
    fn a_playlist_created_here_starts_selected() {
        let conn = mem();
        let me = db::this_device_uid(&conn).unwrap();
        set_mode(&conn, &me, Mode::Selected).unwrap();
        let id = db::create_playlist(&conn, "Nueva", "playlist", None).unwrap();
        let uid: String = conn
            .query_row("SELECT uid FROM playlists WHERE id = ?1", [id], |r| r.get(0))
            .unwrap();

        assert!(!scope_playlists(&conn, &me).unwrap().unwrap().contains(&uid));
        select_new_local(&conn, id).unwrap();
        assert!(scope_playlists(&conn, &me).unwrap().unwrap().contains(&uid));

        // Con el scope en "todo" no escribe nada: no hay nada que marcar.
        let otra = mem();
        let yo = db::this_device_uid(&otra).unwrap();
        let id = db::create_playlist(&otra, "Nueva", "playlist", None).unwrap();
        select_new_local(&otra, id).unwrap();
        assert!(entries(&otra).unwrap().is_empty());
        assert!(scope_playlists(&otra, &yo).unwrap().is_none());
    }

    /// El requisito duro de toda la fase: liberar espacio no puede destruir la
    /// última copia de nada.
    #[test]
    fn a_track_nobody_else_has_is_never_evictable() {
        let conn = mem();
        let me = db::this_device_uid(&conn).unwrap();
        let dir = std::env::temp_dir();
        conn.execute(
            "INSERT INTO tracks (path, uid, content_hash, size_bytes, local_state)
             VALUES (?1, 'solo-aca', 'h1', 100, 'present')",
            [dir.join("x.flac").to_string_lossy()],
        )
        .unwrap();
        set_mode(&conn, &me, Mode::Selected).unwrap();

        // Fuera de scope, pero nadie más lo tiene: no se ofrece.
        assert!(evictable(&conn, &dir).unwrap().is_empty());

        // Con una réplica confirmada en otro dispositivo, sí.
        note_replicas(&conn, "otro", &["h1".to_string()]).unwrap();
        assert_eq!(evictable(&conn, &dir).unwrap().len(), 1);

        // Y una réplica "en mí mismo" no cuenta como respaldo.
        conn.execute("DELETE FROM blob_replicas", []).unwrap();
        note_replicas(&conn, &me, &["h1".to_string()]).unwrap();
        assert!(evictable(&conn, &dir).unwrap().is_empty());
    }
}
