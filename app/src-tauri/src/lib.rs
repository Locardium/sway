mod autosync;
mod cover;
mod db;
mod device_info;
mod discovery;
mod export_xml;
mod hashing;
mod id3_sanitize;
mod import;
mod manifest;
mod merge;
mod pairing;
mod rank;
mod scope;
mod transfer;
mod trash;
mod wire;
mod xml_sync;

// Desktop: Player real (rodio/symphonia, thread propio). Android/iOS: stub
// con la misma API — la reproduccion ahi va por el plugin nativo desde JS
// (ver player_stub.rs y app/src/nativeAudio.ts), este modulo solo existe
// para que AppState y los comandos de playback compilen igual.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod player;
#[cfg(any(target_os = "android", target_os = "ios"))]
#[path = "player_stub.rs"]
mod player;

use player::Player;
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

/// Migración de una sola vez: en Android los tracks vivían en
/// `<carpeta gestionada>/Sway`, un nivel de más — la carpeta gestionada ya es
/// privada de la app (`…/files/Music`), así que ese subdirectorio no separaba
/// de nada. Lo que quedó adentro se sube un nivel.
///
/// Los paths guardados son absolutos: si no se actualizan junto con los
/// archivos, cada track queda apuntando a algo que ya no está ahí. Nunca borra
/// nada — ante un choque de nombres desambigua, que es lo mismo que hace el
/// import.
#[cfg(target_os = "android")]
fn flatten_legacy_subdir(conn: &Connection, music_dir: &std::path::Path) -> anyhow::Result<()> {
    let legacy = music_dir.join("Sway");
    if !legacy.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(&legacy)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let from = entry.path();
        let name = entry.file_name();
        let mut to = music_dir.join(&name);
        if to.exists() {
            let stem = std::path::Path::new(&name)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("track")
                .to_string();
            let ext = std::path::Path::new(&name)
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let mut i = 2;
            loop {
                let cand = if ext.is_empty() {
                    music_dir.join(format!("{stem} ({i})"))
                } else {
                    music_dir.join(format!("{stem} ({i}).{ext}"))
                };
                if !cand.exists() {
                    to = cand;
                    break;
                }
                i += 1;
            }
        }
        std::fs::rename(&from, &to)?;
        conn.execute(
            "UPDATE tracks SET path = ?1 WHERE path = ?2",
            rusqlite::params![to.to_string_lossy(), from.to_string_lossy()],
        )?;
        eprintln!("[lib] migrado: {} -> {}", from.display(), to.display());
    }
    // Solo borra el directorio si quedó vacío.
    std::fs::remove_dir(&legacy).ok();
    Ok(())
}

#[cfg(not(target_os = "android"))]
fn flatten_legacy_subdir(_conn: &Connection, _music_dir: &std::path::Path) -> anyhow::Result<()> {
    Ok(())
}

/// TEMPORAL — diagnóstico.
///
/// Medir la ESPERA ya se hizo y dio ~700 ms con escritura y commit en 0 ms, o
/// sea que el costo no está en quien escribe sino en quien tenía el lock. Esto
/// envuelve el mutex para que cada retención larga se anote sola con el archivo
/// y la línea de quien lo tomó — sin tener que etiquetar a mano los veintipico
/// de lugares que lo usan.
pub struct TrackedDb(Mutex<Connection>);

#[derive(Debug)]
pub struct Poisoned;

impl TrackedDb {
    fn new(c: Connection) -> Self {
        Self(Mutex::new(c))
    }
    /// Misma forma que `Mutex::lock` para que los llamadores no cambien.
    #[track_caller]
    pub fn lock(&self) -> Result<TrackedGuard<'_>, Poisoned> {
        let caller = std::panic::Location::caller();
        let g = self.0.lock().map_err(|_| Poisoned)?;
        Ok(TrackedGuard { g, since: std::time::Instant::now(), caller })
    }
}

pub struct TrackedGuard<'a> {
    g: std::sync::MutexGuard<'a, Connection>,
    since: std::time::Instant,
    caller: &'static std::panic::Location<'static>,
}

impl std::ops::Deref for TrackedGuard<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.g
    }
}
impl std::ops::DerefMut for TrackedGuard<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.g
    }
}
impl Drop for TrackedGuard<'_> {
    fn drop(&mut self) {
        let held = self.since.elapsed().as_millis();
        if held >= 100 {
            perf_line(&format!(
                "LOCK sostenido {held} ms por {}:{}",
                self.caller.file(),
                self.caller.line()
            ));
        }
    }
}

pub struct AppState {
    db: TrackedDb,
    /// Sólo lectura, para lo que dibuja la pantalla. Ver `db::open_read`: es lo
    /// que evita que abrir una playlist espere a que el sync suelte el lock.
    db_read: Mutex<Connection>,
    player: Player,
    covers: Mutex<HashMap<i64, Option<String>>>,
    /// Carpeta gestionada (<Música>/Sway): todo lo importado se copia acá.
    music_dir: PathBuf,
    /// Dispositivos Sway vistos en la red local (Fase 5.1).
    peers: discovery::Peers,
    /// Mantener vivo el daemon es lo que mantiene el servicio publicado.
    mdns: Mutex<Option<mdns_sd::ServiceDaemon>>,
    /// Confirmaciones de pairing esperando que el usuario mire la pantalla.
    pairing: pairing::Pairing,
    /// Archivos que el sync acaba de dejar en la carpeta gestionada.
    ///
    /// El watcher observa esa carpeta y auto-importa lo que aparece — que es
    /// justo lo que NO hay que hacer con algo traído por sync: crearía una
    /// fila con un uid nuevo (rompiendo la identidad compartida entre
    /// dispositivos) y le pisaría la metadata sincronizada con los tags del
    /// archivo. El sync anota el destino antes del rename y el watcher lo
    /// saltea una vez.
    /// Con timestamp y no de a uno: el filesystem emite varios eventos por
    /// archivo (create, modify, close), así que consumir la marca en el
    /// primero dejaba pasar los siguientes — y cada re-import bumpeaba la
    /// fecha del track, dejando a este dispositivo "más nuevo" en cada vuelta.
    expected_paths: Mutex<HashMap<PathBuf, i64>>,
    /// Estado del sync automático (cambios pendientes de propagar).
    autosync: autosync::AutoSync,
}

/// Cuánto tiempo se ignoran los eventos de un archivo que dejó el sync.
/// Cubre la ráfaga de eventos del filesystem sin tapar un cambio real hecho
/// mucho después.
const EXPECTED_PATH_TTL_MS: i64 = 60_000;

impl AppState {
    pub fn expect_path(&self, p: &std::path::Path) {
        let now = db::now_ms();
        let mut map = self.expected_paths.lock().unwrap();
        map.retain(|_, ts| now - *ts < EXPECTED_PATH_TTL_MS);
        map.insert(p.to_path_buf(), now);
    }

    /// True si el path lo escribió el sync hace poco.
    fn is_expected(&self, p: &std::path::Path) -> bool {
        let now = db::now_ms();
        match self.expected_paths.lock().unwrap().get(p) {
            Some(ts) => now - *ts < EXPECTED_PATH_TTL_MS,
            None => false,
        }
    }
}

#[tauri::command]
fn import_folder(app: AppHandle, state: State<AppState>, folder: String) -> Result<usize, String> {
    let n = {
        let conn = state.db.lock().unwrap();
        import::import_folder(&conn, &state.music_dir, &folder, |done, total| {
            let _ = app.emit("import-progress", (done, total));
        })
        .map_err(|e| e.to_string())?
    };
    autosync::note_change(&app);
    Ok(n)
}

/// Avisa cuando algo que corre con el lock de la DB tomado tardó lo suficiente
/// como para que se note en la pantalla. Sin esto, "se traba un segundo" es
/// imposible de atribuir: en cada refresco corren tres consultas distintas y
/// cualquiera de las tres puede ser la cara.
fn timed<T>(what: &str, f: impl FnOnce() -> T) -> T {
    let t0 = std::time::Instant::now();
    let out = f();
    let ms = t0.elapsed().as_millis();
    if ms >= 30 {
        log::info!("[perf] {what}: {ms} ms");
        perf_line(&format!("{what}: {ms} ms"));
    }
    out
}

/// Dónde se dejan los tiempos, además del log. Lo setea el `setup`.
static PERF_FILE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// TEMPORAL — diagnóstico.
///
/// En Android los `log::*` van a logcat, pero hay dispositivos que lo traen
/// apagado a nivel sistema (`live.logcat=disable`, que ni el shell de adb puede
/// cambiar): ahí el buffer entero devuelve cero líneas y no hay forma de ver un
/// tiempo. Un archivo al lado de la DB se puede sacar con `run-as` sin tocar
/// ninguna configuración del teléfono.
pub fn perf_line(line: &str) {
    use std::io::Write;
    let Some(path) = PERF_FILE.get() else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{} {line}", db::now_ms());
    }
}

#[tauri::command]
fn list_tracks(state: State<AppState>) -> Result<Vec<db::Track>, String> {
    let conn = state.db_read.lock().unwrap();
    timed("list_tracks", || {
        let mut tracks = db::list_tracks(&conn).map_err(|e| e.to_string())?;
        timed("  mark_scope", || mark_scope(&conn, &mut tracks));
        Ok(tracks)
    })
}

/// Marca qué tracks entran en lo que este dispositivo sincroniza. La tabla los
/// muestra atenuados: sin eso, desmarcar una playlist no se nota en ningún
/// lado hasta que alguien libera espacio.
fn mark_scope(conn: &Connection, tracks: &mut [db::Track]) {
    let me = match db::this_device_uid(conn) {
        Ok(v) => v,
        Err(_) => return,
    };
    let Ok(Some(in_scope)) = scope::scope_tracks(conn, &me) else {
        return; // scope = todo (o error): queda el default, todo adentro
    };
    for t in tracks.iter_mut() {
        t.in_scope = t.uid.as_deref().map(|u| in_scope.contains(u)).unwrap_or(true);
    }
}

#[tauri::command]
fn import_files(
    app: AppHandle,
    state: State<AppState>,
    paths: Vec<String>,
) -> Result<Vec<i64>, String> {
    let ids = {
        let conn = state.db.lock().unwrap();
        import::import_paths(&conn, &state.music_dir, &paths, |done, total| {
            let _ = app.emit("import-progress", (done, total));
        })
        .map_err(|e| e.to_string())?
    };
    autosync::note_change(&app);
    Ok(ids)
}

/// Import desde un picker de archivos (Android/iOS): el picker da un
/// `content://` (o `file://`) URI, no un path de filesystem crudo que
/// `std::fs` pueda abrir directo. `tauri_plugin_fs`'s `Fs::read` sabe
/// resolver esos URIs (soporte explicito para Android en el crate),
/// asi que la lectura pasa por ahi en vez de por std::fs.
#[tauri::command]
fn import_from_uri(app: AppHandle, state: State<AppState>, uri: String, name: String) -> Result<i64, String> {
    use std::str::FromStr;
    use tauri_plugin_fs::{FilePath, FsExt};
    // app.path().file_name() resuelve el nombre real via la API nativa de
    // Android para content:// (Tauri lo documenta explicitamente para esto).
    // Mucho mas confiable que adivinar por texto del URI del lado del
    // frontend — `name` (lo que mando el picker) queda solo de fallback.
    let resolved_name = app.path().file_name(&uri).unwrap_or(name);
    let file_path = FilePath::from_str(&uri).map_err(|e| e.to_string())?;
    let bytes = app.fs().read(file_path).map_err(|e| e.to_string())?;
    let id = {
        let conn = state.db.lock().unwrap();
        import::import_bytes(&conn, &state.music_dir, &resolved_name, &bytes)
            .map_err(|e| e.to_string())?
    };
    autosync::note_change(&app);
    Ok(id)
}

#[tauri::command]
fn track_playlists(state: State<AppState>, id: i64) -> Result<Vec<i64>, String> {
    let conn = state.db_read.lock().unwrap();
    db::track_playlists(&conn, id).map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_tracks(app: AppHandle, state: State<AppState>, ids: Vec<i64>) -> Result<(), String> {
    let music = state.music_dir.clone();
    {
        let mut conn = state.db.lock().unwrap();
        db::delete_tracks(&mut conn, &music, &ids).map_err(|e| e.to_string())?;
    }
    autosync::note_change(&app);
    Ok(())
}

#[tauri::command]
fn set_volume(state: State<AppState>, volume: f32) {
    state.player.set_volume(volume);
}

#[tauri::command]
fn cover_thumb(state: State<AppState>, id: i64) -> Result<Option<String>, String> {
    if let Some(c) = state.covers.lock().unwrap().get(&id) {
        return Ok(c.clone());
    }
    let path = {
        let conn = state.db.lock().unwrap();
        db::track_path(&conn, id).map_err(|e| e.to_string())?
    };
    let thumb = cover::thumb_data_url(std::path::Path::new(&path));
    state.covers.lock().unwrap().insert(id, thumb.clone());
    Ok(thumb)
}

/// Resuelve id -> path del archivo. En desktop `play_track` ya lo hace
/// internamente en Rust; en Android el JS necesita el path para pasarselo al
/// plugin nativo (`setSource`), asi que hace falta exponerlo como comando.
#[tauri::command]
fn get_track_path(state: State<AppState>, id: i64) -> Result<String, String> {
    let conn = state.db.lock().unwrap();
    db::track_path(&conn, id).map_err(|e| e.to_string())
}

#[tauri::command]
fn reveal_track(state: State<AppState>, id: i64) -> Result<(), String> {
    let path = {
        let conn = state.db.lock().unwrap();
        db::track_path(&conn, id).map_err(|e| e.to_string())?
    };
    #[cfg(target_os = "windows")]
    std::process::Command::new("explorer")
        .arg(format!("/select,{path}"))
        .spawn()
        .map_err(|e| e.to_string())?;
    #[cfg(target_os = "macos")]
    std::process::Command::new("open")
        .args(["-R", &path])
        .spawn()
        .map_err(|e| e.to_string())?;
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let dir = std::path::Path::new(&path).parent().unwrap_or(std::path::Path::new("/"));
        std::process::Command::new("xdg-open")
            .arg(dir)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn list_playlists(state: State<AppState>) -> Result<Vec<db::PlaylistNode>, String> {
    let conn = state.db_read.lock().unwrap();
    timed("list_playlists", || {
        let mut nodes = db::list_playlists(&conn).map_err(|e| e.to_string())?;
        timed("  mark_playlist_scope", || mark_playlist_scope(&conn, &mut nodes));
        Ok(nodes)
    })
}

/// Marca qué playlists entran en lo que este dispositivo sincroniza. El árbol
/// de la vista principal apaga las que no y esconde las que además ya se
/// liberaron; el editor de scope las muestra todas igual, o no habría manera de
/// volver a marcarlas.
fn mark_playlist_scope(conn: &Connection, nodes: &mut [db::PlaylistNode]) {
    let Ok(me) = db::this_device_uid(conn) else { return };
    let Ok(Some(selected)) = scope::scope_playlists(conn, &me) else {
        return; // scope = todo (o error): queda el default, todo adentro
    };
    let stranded = scope::stranded_counts(conn, &me).unwrap_or_default();
    for n in nodes.iter_mut() {
        n.in_scope = n.uid.as_deref().map(|u| selected.contains(u)).unwrap_or(true);
        n.stranded_count = stranded.get(&n.id).copied().unwrap_or(0);
    }
}

#[tauri::command]
fn create_playlist(
    app: AppHandle,
    state: State<AppState>,
    name: String,
    kind: String,
    parent_id: Option<i64>,
) -> Result<i64, String> {
    let id = {
        let conn = state.db.lock().unwrap();
        let id = db::create_playlist(&conn, &name, &kind, parent_id).map_err(|e| e.to_string())?;
        // La creó el usuario acá: que no se esconda al instante por no estar
        // marcada. No fallar el comando si esto no sale — la playlist ya existe.
        if let Err(e) = scope::select_new_local(&conn, id) {
            log::warn!("[scope] no se pudo marcar la playlist nueva: {e}");
        }
        id
    };
    autosync::note_change(&app);
    Ok(id)
}

#[tauri::command]
fn rename_playlist(app: AppHandle, state: State<AppState>, id: i64, name: String) -> Result<(), String> {
    {
        let conn = state.db.lock().unwrap();
        db::rename_playlist(&conn, id, &name).map_err(|e| e.to_string())?;
    }
    autosync::note_change(&app);
    Ok(())
}

#[tauri::command]
fn delete_playlist(app: AppHandle, state: State<AppState>, id: i64) -> Result<(), String> {
    {
        let conn = state.db.lock().unwrap();
        db::delete_playlist(&conn, id).map_err(|e| e.to_string())?;
    }
    autosync::note_change(&app);
    Ok(())
}

#[tauri::command]
fn move_playlist(
    app: AppHandle,
    state: State<AppState>,
    id: i64,
    parent_id: Option<i64>,
    index: i64,
) -> Result<(), String> {
    {
        let mut conn = state.db.lock().unwrap();
        db::move_playlist(&mut conn, id, parent_id, index)?;
    }
    autosync::note_change(&app);
    Ok(())
}

#[tauri::command]
fn playlist_tracks(state: State<AppState>, playlist_id: i64) -> Result<Vec<db::Track>, String> {
    let conn = state.db_read.lock().unwrap();
    timed("playlist_tracks", || {
        let mut tracks = db::playlist_tracks(&conn, playlist_id).map_err(|e| e.to_string())?;
        // Acotado a esta playlist: marcar veinte filas no puede costar lo mismo
        // que recorrer la biblioteca entera.
        timed("  mark_scope(playlist)", || {
            if let Ok(me) = db::this_device_uid(&conn) {
                if let Ok(Some(in_scope)) = scope::scope_tracks_of_playlist(&conn, &me, playlist_id)
                {
                    for t in tracks.iter_mut() {
                        t.in_scope =
                            t.uid.as_deref().map(|u| in_scope.contains(u)).unwrap_or(true);
                    }
                }
            }
        });
        Ok(tracks)
    })
}

#[tauri::command]
fn add_tracks_to_playlist(
    app: AppHandle,
    state: State<AppState>,
    playlist_id: i64,
    track_ids: Vec<i64>,
) -> Result<usize, String> {
    let n = {
        let mut conn = state.db.lock().unwrap();
        db::add_tracks_to_playlist(&mut conn, playlist_id, &track_ids).map_err(|e| e.to_string())?
    };
    autosync::note_change(&app);
    Ok(n)
}

#[tauri::command]
fn remove_tracks_from_playlist(
    app: AppHandle,
    state: State<AppState>,
    playlist_id: i64,
    track_ids: Vec<i64>,
) -> Result<(), String> {
    {
        let mut conn = state.db.lock().unwrap();
        db::remove_tracks_from_playlist(&mut conn, playlist_id, &track_ids)
            .map_err(|e| e.to_string())?;
    }
    autosync::note_change(&app);
    Ok(())
}

#[tauri::command]
fn reorder_playlist_tracks(
    app: AppHandle,
    state: State<AppState>,
    playlist_id: i64,
    track_ids: Vec<i64>,
    index: i64,
) -> Result<(), String> {
    {
        let mut conn = state.db.lock().unwrap();
        db::reorder_playlist_tracks(&mut conn, playlist_id, &track_ids, index)
            .map_err(|e| e.to_string())?;
    }
    autosync::note_change(&app);
    Ok(())
}

#[tauri::command]
fn play_track(state: State<AppState>, id: i64) -> Result<(), String> {
    let path = {
        let conn = state.db.lock().unwrap();
        db::track_path(&conn, id).map_err(|e| e.to_string())?
    };
    state.player.play(std::path::PathBuf::from(path));
    Ok(())
}

#[tauri::command]
fn pause_playback(state: State<AppState>) {
    state.player.pause();
}

#[tauri::command]
fn resume_playback(state: State<AppState>) {
    state.player.resume();
}

#[tauri::command]
fn stop_playback(state: State<AppState>) {
    state.player.stop();
}

#[tauri::command]
fn seek_to(state: State<AppState>, secs: u64) {
    state.player.seek(secs);
}

#[tauri::command]
fn playback_position(state: State<AppState>) -> u64 {
    state.player.position_secs()
}

/// Identidad de este dispositivo, para mostrarla y poder renombrarla en
/// Settings. El uid no se puede cambiar (lo referencian tombstones y clocks);
/// el nombre es solo para reconocerlo en la lista de dispositivos del otro
/// lado.
#[tauri::command]
fn device_identity(state: State<AppState>) -> Result<(String, String), String> {
    let conn = state.db.lock().unwrap();
    let uid = db::this_device_uid(&conn).map_err(|e| e.to_string())?;
    let name = db::device_name(&conn).map_err(|e| e.to_string())?;
    Ok((uid, name))
}

#[tauri::command]
fn set_device_name(state: State<AppState>, name: String) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    db::set_device_name(&conn, &name).map_err(|e| e.to_string())
}

/// Dispositivos Sway visibles en la red local. El frontend lo llama al abrir
/// la sección de sync y cada vez que llega el evento `peers-changed`.
#[tauri::command]
fn list_peers(state: State<AppState>) -> Vec<discovery::Peer> {
    let conn = state.db.lock().unwrap();
    state.peers.merged_list(&conn)
}

/// El botón "Refresh": consulta la red de nuevo (mDNS) y recomprueba en el
/// acto quién está alcanzable, sin esperar el sondeo periódico. Releer la
/// lista local no alcanzaba — ya estaba al día; lo que faltaba era preguntar
/// afuera.
///
/// Las respuestas de mDNS llegan por el evento `peers-changed`, no en el
/// retorno: el protocolo es asincrónico y los peers contestan cuando
/// contestan. El sondeo sí es inmediato.
///
/// Ojo: esto NO cambia quién está vinculado. El pairing vive en `devices` de
/// cada dispositivo y se sincroniza avisando (ver `pairing::unpair`), no
/// mirando la red.
#[tauri::command]
fn refresh_peers(app: AppHandle) -> Result<(), String> {
    discovery::refresh(&app).map_err(|e| e.to_string())?;
    let handle = app.clone();
    std::thread::spawn(move || discovery::probe_once(&handle));
    Ok(())
}

/// Vincula con un peer, o si ya está vinculado le pide sus conteos de
/// biblioteca. Vuelve enseguida: el resultado llega por los eventos
/// `pairing-request` / `pairing-done` / `peer-hello`, porque del otro lado
/// puede haber una persona tardando en confirmar.
#[tauri::command]
fn connect_peer(app: AppHandle, uid: String) {
    pairing::connect_peer(app, uid);
}

/// Simulacro de sync: compara las dos bibliotecas y publica por el evento
/// `sync-plan` lo que pasaría. No escribe nada — la transferencia real es 5.4.
#[tauri::command]
fn preview_sync(app: AppHandle, uid: String) {
    pairing::preview_sync(app, uid);
}

/// Ejecuta la transferencia de archivos del plan. Sólo archivos: metadata,
/// playlists y borrados llegan en 5.5/5.6. El progreso va por `sync-progress`
/// y el resumen final por `sync-done`.
#[tauri::command]
fn sync_files(app: AppHandle, uid: String) {
    pairing::sync_files(app, uid);
}

/// Respuesta del usuario al código de verificación.
#[tauri::command]
fn confirm_pairing(app: AppHandle, uid: String, accept: bool) -> bool {
    pairing::resolve_decision(&app, &uid, accept)
}

#[tauri::command]
fn unpair_device(app: AppHandle, uid: String) -> Result<(), String> {
    pairing::unpair(&app, &uid).map_err(|e| e.to_string())?;
    let _ = app.emit("peers-changed", ());
    Ok(())
}

// ---------------------------------------------------------------------------
// Políticas, scope selectivo y espacio (Fase 5.7)
// ---------------------------------------------------------------------------

/// Qué hago con los borrados que manda ese dispositivo. Es lo único de a pares
/// y **local**: protege ESTA biblioteca, así que sólo se edita acá (la
/// dirección, en cambio, es del dispositivo y se replica — ver `scope.rs`).
#[tauri::command]
fn get_delete_policy(state: State<AppState>, uid: String) -> Result<String, String> {
    let conn = state.db.lock().unwrap();
    Ok(conn
        .query_row(
            "SELECT deletes FROM sync_policy WHERE device_uid = ?1",
            [&uid],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "propagate".to_string()))
}

#[tauri::command]
fn set_delete_policy(state: State<AppState>, uid: String, deletes: String) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    conn.execute(
        "INSERT INTO sync_policy (device_uid, deletes) VALUES (?1, ?2)
         ON CONFLICT(device_uid) DO UPDATE SET deletes = excluded.deletes",
        rusqlite::params![uid, deletes],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ScopeView {
    mode: String,
    /// both | send | receive | off — qué hace ESE dispositivo.
    direction: String,
    /// Uids de playlists/carpetas marcadas a mano. Lo que cuelga de una
    /// carpeta marcada entra sin figurar acá — el árbol lo resuelve el backend
    /// (ver `scope::expand`), no la UI.
    selected: Vec<String>,
}

#[tauri::command]
fn get_scope(state: State<AppState>, device_uid: String) -> Result<ScopeView, String> {
    let conn = state.db.lock().unwrap();
    let s = scope::get(&conn, &device_uid).map_err(|e| e.to_string())?;
    let direction = match (s.direction.sends, s.direction.receives) {
        (true, true) => "both",
        (true, false) => "send",
        (false, true) => "receive",
        (false, false) => "off",
    };
    Ok(ScopeView {
        mode: s.mode.as_str().to_string(),
        direction: direction.into(),
        selected: s.selected.into_iter().collect(),
    })
}

/// Qué hace un dispositivo: manda, recibe, las dos, o nada. Se edita desde
/// cualquier lado — es dato replicado, igual que el scope.
#[tauri::command]
fn set_scope_direction(
    app: AppHandle,
    state: State<AppState>,
    device_uid: String,
    direction: String,
) -> Result<(), String> {
    {
        let conn = state.db.lock().unwrap();
        scope::set_direction(&conn, &device_uid, &direction).map_err(|e| e.to_string())?;
    }
    after_scope_change(&app, &device_uid);
    Ok(())
}

/// El scope es dato replicado: cambiarlo acá dispara un sync, así que el
/// dispositivo afectado se entera aunque el cambio se haya hecho en otro.
#[tauri::command]
fn set_scope_mode(
    app: AppHandle,
    state: State<AppState>,
    device_uid: String,
    mode: String,
) -> Result<(), String> {
    {
        let conn = state.db.lock().unwrap();
        scope::set_mode(&conn, &device_uid, scope::Mode::from_setting(&mode))
            .map_err(|e| e.to_string())?;
    }
    after_scope_change(&app, &device_uid);
    Ok(())
}

/// Un cambio de scope tiene dos efectos inmediatos: hay que propagarlo (es
/// dato replicado) y, si el cambio es del scope de ESTE dispositivo, hay que
/// rescatar de la papelera lo que acaba de volver a entrar — antes de que el
/// próximo sync lo pida por la red.
/// Cuánto se espera, desde el último cambio de scope, antes de hacer lo caro.
///
/// La fila se escribe en el acto: son microsegundos y es lo que hace que la
/// pantalla no mienta. Lo que espera es el after: recargar la biblioteca entera
/// por IPC, escanear la papelera (que hashea) y despertar el sync. Marcar una
/// carpeta escribe una fila por playlist de adentro, y hacer ese trabajo por
/// fila significaba varias recargas completas y varios escaneos peleando por el
/// mismo lock de SQLite — la pantalla se trababa en cada tilde.
///
/// Corto a propósito. La espera larga tenía sentido cuando esto era caro y se
/// pagaba por fila; ahora un click es una transacción y un after, así que
/// alargarla sólo demora el refresco de la biblioteca de atrás sin ahorrar
/// nada. Lo justo para que dos tildes seguidos se junten en un solo refresco.
const SCOPE_SETTLE_MS: u64 = 400;
/// Cuánto espera el SYNC después de un cambio de scope. Mucho más que el
/// refresco de la pantalla, y a propósito.
///
/// `note_change` despierta al autosync, y un sync no es barato: arma el
/// manifest con el lock tomado, habla por red, aplica el merge con el lock
/// tomado y al terminar emite `library-changed`, que fuerza a la UI a recargar
/// todo. Medido en escritorio da alrededor de un segundo; en el celular, más.
/// Encadenado a cada tilde, eso era el freeze: no las consultas, el sync.
///
/// Marcar playlists es una ráfaga —se abre el panel, se tildan varias, se
/// cierra—, así que esperar a que la persona termine no atrasa nada que se
/// note. Lo único que se demora es que el otro dispositivo se entere, y eso
/// nadie lo está mirando.
const SCOPE_SYNC_SETTLE_MS: u64 = 6000;
static SCOPE_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SCOPE_SYNC_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn after_scope_change(app: &AppHandle, device_uid: &str) {
    use std::sync::atomic::Ordering;
    let mine = {
        let state = app.state::<AppState>();
        let conn = state.db.lock().unwrap();
        db::this_device_uid(&conn).map(|me| me == device_uid).unwrap_or(false)
    };

    // Refrescar la pantalla: corto, es lo único que la persona está esperando.
    if mine {
        let mark = SCOPE_GEN.fetch_add(1, Ordering::SeqCst) + 1;
        let app = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(SCOPE_SETTLE_MS));
            // Entró otro click mientras dormía: que lo haga el último, una vez.
            if SCOPE_GEN.load(Ordering::SeqCst) != mark {
                return;
            }
            // El rescate primero: toma el lock y puede hashear. Emitir antes
            // largaba a la UI a pedir la biblioteca entera justo contra ese
            // lock, y las dos cosas se esperaban entre sí.
            timed("restore_local", || pairing::restore_local(&app));
            // Marcar o desmarcar cambia qué se ve —qué playlists están en el
            // árbol, qué temas se muestran y cuáles quedan apagados— y eso sale
            // de `list_playlists` / `list_tracks`, que la UI no vuelve a pedir
            // sola. Sin este evento el cambio no se notaba hasta reiniciar.
            let _ = app.emit("library-changed", ());
        });
    }

    // Propagar al otro dispositivo: largo, no lo está esperando nadie.
    let mark = SCOPE_SYNC_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(SCOPE_SYNC_SETTLE_MS));
        if SCOPE_SYNC_GEN.load(Ordering::SeqCst) != mark {
            return;
        }
        autosync::note_change(&app);
    });
}

#[tauri::command]
fn set_scope_playlist(
    app: AppHandle,
    state: State<AppState>,
    device_uid: String,
    playlist_uid: String,
    selected: bool,
) -> Result<(), String> {
    {
        let conn = state.db.lock().unwrap();
        scope::set_playlist(&conn, &device_uid, &playlist_uid, selected)
            .map_err(|e| e.to_string())?;
    }
    after_scope_change(&app, &device_uid);
    Ok(())
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScopeChange {
    uid: String,
    on: bool,
}

/// Varias filas de scope de una sola vez.
///
/// Marcar una carpeta cambia una fila por playlist de adentro. Mandarlas de a
/// una era un round trip de IPC, una transacción implícita (o sea un fsync) y
/// un `after_scope_change` por cada una: con una carpeta grande, el tilde
/// tardaba en soltar y el siguiente click quedaba esperando. Acá es un viaje,
/// un commit y un solo after.
#[tauri::command]
fn set_scope_playlists(
    app: AppHandle,
    state: State<AppState>,
    device_uid: String,
    changes: Vec<ScopeChange>,
) -> Result<(), String> {
    {
        // TEMPORAL — diagnóstico. Es el camino que dispara el click, y el único
        // que todavía toma el lock de escritura: interesa separar la espera del
        // commit.
        let t0 = std::time::Instant::now();
        let mut conn = state.db.lock().unwrap();
        let lock_ms = t0.elapsed().as_millis();
        let t1 = std::time::Instant::now();
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        for c in &changes {
            scope::set_playlist(&tx, &device_uid, &c.uid, c.on).map_err(|e| e.to_string())?;
        }
        let t2 = std::time::Instant::now();
        tx.commit().map_err(|e| e.to_string())?;
        let (write_ms, commit_ms) = (t1.elapsed().as_millis(), t2.elapsed().as_millis());
        if lock_ms + write_ms + commit_ms >= 30 {
            perf_line(&format!(
                "set_scope_playlists: lock {lock_ms} ms, escritura {write_ms} ms, commit {commit_ms} ms, {} fila(s)",
                changes.len()
            ));
        }
    }
    after_scope_change(&app, &device_uid);
    Ok(())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct StorageView {
    /// Bytes de los archivos que están en este dispositivo.
    library_bytes: i64,
    tracks_present: i64,
    /// Filas cuyo archivo ya no está acá (evacuadas por sync selectiva).
    tracks_absent: i64,
    /// Lo que se puede liberar ahora mismo sin arriesgar nada: fuera de scope
    /// y con copia confirmada en otro dispositivo vinculado.
    freeable_count: i64,
    freeable_bytes: i64,
}

#[tauri::command]
fn storage_status(state: State<AppState>) -> Result<StorageView, String> {
    let conn = state.db_read.lock().unwrap();
    let (library_bytes, tracks_present): (i64, i64) = conn
        .query_row(
            "SELECT COALESCE(SUM(size_bytes), 0), COUNT(*) FROM tracks
             WHERE local_state = 'present'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| e.to_string())?;
    let tracks_absent: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tracks WHERE local_state <> 'present'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let items = timed("storage_status/evictable", || {
        scope::evictable(&conn, &state.music_dir)
    })
    .map_err(|e| e.to_string())?;
    Ok(StorageView {
        library_bytes,
        tracks_present,
        tracks_absent,
        freeable_count: items.len() as i64,
        freeable_bytes: items.iter().map(|i| i.size).sum(),
    })
}

/// Libera espacio: los archivos fuera de scope se van a la papelera de la
/// biblioteca (30 días) y sus filas quedan en `absent`. Nada se borra de la
/// biblioteca — los tracks se siguen viendo, en gris, y re-marcar su playlist
/// los baja de nuevo.
#[tauri::command]
fn free_space(app: AppHandle, state: State<AppState>) -> Result<(i64, i64), String> {
    let (n, bytes) = {
        let conn = state.db.lock().unwrap();
        let items = scope::evictable(&conn, &state.music_dir).map_err(|e| e.to_string())?;
        scope::evict(&conn, &state.music_dir, &items).map_err(|e| e.to_string())?
    };
    if n > 0 {
        let _ = app.emit("library-changed", ());
    }
    Ok((n as i64, bytes))
}

/// Cola de borrados esperando confirmación (política `deletes = 'ask'`).
#[tauri::command]
fn list_pending_deletes(state: State<AppState>) -> Result<Vec<merge::PendingDelete>, String> {
    let conn = state.db.lock().unwrap();
    merge::pending_deletes(&conn).map_err(|e| e.to_string())
}

#[tauri::command]
fn resolve_pending_delete(
    app: AppHandle,
    state: State<AppState>,
    id: i64,
    accept: bool,
) -> Result<(), String> {
    {
        let conn = state.db.lock().unwrap();
        merge::resolve_pending_delete(&conn, &state.music_dir, id, accept)
            .map_err(|e| e.to_string())?;
    }
    let _ = app.emit("library-changed", ());
    Ok(())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LogEntry {
    ts: i64,
    kind: String,
    detail: String,
}

/// Historial de un dispositivo (vinculación, syncs, conflictos).
#[tauri::command]
fn sync_history(state: State<AppState>, uid: String, limit: i64) -> Result<Vec<LogEntry>, String> {
    let conn = state.db.lock().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT ts, kind, detail FROM sync_log WHERE peer = ?1
             ORDER BY ts DESC LIMIT ?2",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(rusqlite::params![uid, limit], |r| {
            Ok(LogEntry { ts: r.get(0)?, kind: r.get(1)?, detail: r.get(2)? })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| e.to_string())
}

/// "Sync now" manual: escribe el XML sin importar el estado del toggle.
#[tauri::command]
fn export_library_xml_now(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    xml_sync::write_now(&app, &conn, &state.music_dir).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_auto_sync_xml(state: State<AppState>) -> Result<bool, String> {
    let conn = state.db.lock().unwrap();
    db::get_auto_sync_xml(&conn).map_err(|e| e.to_string())
}

#[tauri::command]
fn set_auto_sync_xml(state: State<AppState>, enabled: bool) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    db::set_auto_sync_xml(&conn, enabled).map_err(|e| e.to_string())
}

/// Lo llama el frontend despues de cada mutacion (import, playlists, tracks).
/// El backend decide si hace algo: si el toggle esta apagado, no-op.
///
/// Solo el XML: el sync P2P se entera por su cuenta, desde los comandos que
/// mutan la biblioteca. Avisarle tambien desde aca lo disparaba dos veces por
/// operacion, porque el frontend llama a esto DESPUES de cada mutacion.
#[tauri::command]
fn sync_xml_after_change(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    xml_sync::write_if_enabled(&app, &conn, &state.music_dir);
    Ok(())
}

#[tauri::command]
fn get_auto_sync_p2p(state: State<AppState>) -> bool {
    let conn = state.db.lock().unwrap();
    autosync::enabled(&conn)
}

#[tauri::command]
fn set_auto_sync_p2p(state: State<AppState>, enabled: bool) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    autosync::set_enabled(&conn, enabled).map_err(|e| e.to_string())
}

/// Completa `rel_path` (nombre del archivo dentro de la carpeta gestionada)
/// en las filas que no lo tengan. `path` es absoluto y por lo tanto local:
/// `E:\...\Sway\x.flac` en la PC y `/storage/.../Music/x.flac` en el celu. Lo
/// que viaja entre dispositivos es `rel_path`; el path absoluto se rearma en
/// cada uno. Los tracks legacy de afuera de la carpeta gestionada quedan sin
/// `rel_path` — no son sincronizables hasta que se los reimporte.
fn backfill_rel_paths(conn: &Connection, music_dir: &std::path::Path) -> rusqlite::Result<usize> {
    let rows: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT id, path FROM tracks WHERE rel_path IS NULL")?;
        let r = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        r.collect::<rusqlite::Result<_>>()?
    };
    let mut done = 0;
    for (id, path) in rows {
        let p = std::path::Path::new(&path);
        if let Ok(rel) = p.strip_prefix(music_dir) {
            conn.execute(
                "UPDATE tracks SET rel_path = ?1 WHERE id = ?2",
                rusqlite::params![rel.to_string_lossy(), id],
            )?;
            done += 1;
        }
    }
    Ok(done)
}

/// Hashea en segundo plano los tracks que todavia no tienen `content_hash`.
///
/// Corre en su propio thread y **suelta el lock de la DB entre archivo y
/// archivo**: con 100 GB de biblioteca esto tarda minutos, y sostener el mutex
/// dejaria la UI congelada todo ese tiempo. El hash en si se calcula fuera del
/// lock; adentro solo entran el SELECT inicial y el UPDATE de cada fila.
///
/// Reanudable por construccion: el criterio de "falta" esta en la DB, asi que
/// cerrar la app a mitad solo deja pendiente lo que no llego a hashearse.
fn spawn_hash_backfill(handle: AppHandle) {
    std::thread::spawn(move || {
        let state = handle.state::<AppState>();
        let pending = {
            let conn = state.db.lock().unwrap();
            hashing::pending(&conn).unwrap_or_default()
        };
        if pending.is_empty() {
            return;
        }
        let total = pending.len();
        eprintln!("[hash] {total} tracks sin hashear");
        for (i, (id, path)) in pending.into_iter().enumerate() {
            let p = std::path::PathBuf::from(&path);
            let stamp = hashing::file_stamp(&p);
            let hash = match stamp {
                Ok(_) => hashing::hash_file(&p).ok(),
                Err(_) => None, // archivo faltante: se reintenta en el proximo arranque
            };
            if let (Ok((size, mtime)), Some(hash)) = (stamp, hash) {
                let conn = state.db.lock().unwrap();
                let _ = conn.execute(
                    "UPDATE tracks SET content_hash = ?1, size_bytes = ?2, mtime_ms = ?3
                     WHERE id = ?4",
                    rusqlite::params![hash, size, mtime, id],
                );
            }
            if (i + 1) % 25 == 0 || i + 1 == total {
                let _ = handle.emit("hash-progress", (i + 1, total));
            }
        }
        eprintln!("[hash] backfill completo");
        let _ = handle.emit("hash-progress", (total, total));
    });
}

/// Observa la carpeta gestionada y auto-importa archivos de audio nuevos.
/// Corre en su propio thread; emite `library-changed` cuando cambia el conteo.
fn spawn_folder_watch(handle: AppHandle, dir: PathBuf) {
    use notify_debouncer_mini::new_debouncer;
    use notify_debouncer_mini::notify::RecursiveMode;
    use std::time::Duration;

    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut debouncer = match new_debouncer(Duration::from_millis(900), tx) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[watch] no se pudo iniciar: {e}");
                return;
            }
        };
        if let Err(e) = debouncer.watcher().watch(&dir, RecursiveMode::Recursive) {
            eprintln!("[watch] watch fallo: {e}");
            return;
        }
        eprintln!("[watch] observando {}", dir.display());
        for res in rx {
            let events = match res {
                Ok(ev) => ev,
                Err(_) => continue,
            };
            let state = handle.state::<AppState>();
            let paths: Vec<String> = events
                .into_iter()
                // Lo que dejó el sync ya está en la DB con su uid del otro
                // dispositivo: reimportarlo sería duplicarlo mal.
                .filter(|e| !state.is_expected(&e.path))
                // Y lo que hay en .sway-trash / .sway-incoming no es
                // biblioteca: es lo que se acaba de borrar y lo que se está
                // bajando. Auto-importarlo resucitaría cada borrado.
                .filter(|e| !import::is_internal_path(&e.path))
                .map(|e| e.path.to_string_lossy().into_owned())
                .collect();
            if paths.is_empty() {
                continue;
            }
            // Un archivo que ya está en la biblioteca no tiene nada que
            // importar, y averiguarlo es una consulta por path.
            //
            // Sin esto, cada evento del watcher entraba al lock de ESCRITURA y
            // leía los tags de todo lo que venía en la tanda. En Android la
            // carpeta gestionada vive en almacenamiento emulado y el watcher
            // dispara sin parar sobre los mismos archivos: medido en el
            // dispositivo, retenciones de 200 a 1500 ms encadenadas, para no
            // importar nada. Todo lo demás —abrir una playlist, marcar en el
            // scope— hacía cola detrás de eso.
            let paths: Vec<String> = {
                let conn = state.db_read.lock().unwrap();
                let mut stmt = match conn.prepare_cached("SELECT 1 FROM tracks WHERE path = ?1") {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                paths
                    .into_iter()
                    .filter(|p| !stmt.exists([p]).unwrap_or(false))
                    .collect()
            };
            if paths.is_empty() {
                continue;
            }
            let changed = {
                let conn = state.db.lock().unwrap();
                let before: i64 = conn
                    .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
                    .unwrap_or(0);
                let _ = import::import_paths(&conn, &state.music_dir, &paths, |_, _| {});
                let after: i64 = conn
                    .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
                    .unwrap_or(before);
                if after != before {
                    xml_sync::write_if_enabled(&handle, &conn, &state.music_dir);
                }
                after != before
            };
            if changed {
                eprintln!("[watch] importados nuevos ({} rutas), avisando UI", paths.len());
                autosync::note_change(&handle);
                let _ = handle.emit("library-changed", ());
            }
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Desktop: sin esto los log::* del sync no se ven en ningun lado.
    // RUST_LOG=debug para mas detalle; por default, info.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let _ = env_logger::Builder::from_env(
            env_logger::Env::default().default_filter_or("info"),
        )
        .try_init();
    }

    #[cfg(target_os = "android")]
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Debug)
            .with_tag("sway"),
    );

    #[allow(unused_mut)] // mut solo hace falta en el cfg de abajo (android/ios)
    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_os::init());
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        builder = builder.plugin(tauri_plugin_native_audio::init());
    }
    builder
        .setup(|app| {
            let dir = app.path().app_data_dir().expect("app data dir");
            std::fs::create_dir_all(&dir).ok();
            let db_file = dir.join("sway.sqlite");
            // TEMPORAL — ver `perf_line`. Se trunca en cada arranque para que
            // lo que se lea sea siempre de la corrida actual.
            let perf = dir.join("perf.log");
            std::fs::write(&perf, "").ok();
            let _ = PERF_FILE.set(perf);
            eprintln!("[db] archivo: {}", db_file.display());
            let conn = db::open(&db_file).expect("db open");
            // El WAL se consolida aparte: ver `db::open`, ningún click paga eso.
            db::spawn_checkpointer(&db_file);
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
                .unwrap_or(-1);
            eprintln!("[db] tracks al iniciar: {n}");
            // Carpeta gestionada. En desktop `audio_dir()` es la carpeta de
            // música del usuario, compartida con todo lo demás, así que Sway se
            // queda en su propio subdirectorio. En Android ya es una carpeta
            // privada de la app (`getExternalFilesDir(DIRECTORY_MUSIC)`, o sea
            // `…/files/Music`): anidar otro "Sway" adentro solo agrega un nivel
            // que no separa de nada.
            let audio_dir = app.path().audio_dir().unwrap_or_else(|_| dir.clone());
            let music_dir = if cfg!(target_os = "android") {
                audio_dir
            } else {
                audio_dir.join("Sway")
            };
            std::fs::create_dir_all(&music_dir).ok();
            if let Err(e) = flatten_legacy_subdir(&conn, &music_dir) {
                eprintln!("[lib] migración de la carpeta gestionada falló: {e}");
            }
            eprintln!("[lib] carpeta gestionada: {}", music_dir.display());
            match backfill_rel_paths(&conn, &music_dir) {
                Ok(n) if n > 0 => eprintln!("[lib] rel_path completado en {n} tracks"),
                Err(e) => eprintln!("[lib] backfill de rel_path fallo: {e}"),
                _ => {}
            }
            // Identidad de este dispositivo: se genera una sola vez y no
            // cambia mas (firma tombstones y desempata los LWW).
            match (db::this_device_uid(&conn), db::device_name(&conn)) {
                (Ok(uid), Ok(name)) => eprintln!("[sync] este dispositivo: {name} ({uid})"),
                (a, b) => eprintln!("[sync] no se pudo fijar la identidad: {a:?} / {b:?}"),
            }
            // Puerto efímero reservado por el SO: se anuncia por mDNS y es
            // donde escucha el servidor de sync.
            let listener = std::net::TcpListener::bind("0.0.0.0:0").ok();
            let sync_port = listener
                .as_ref()
                .and_then(|l| l.local_addr().ok())
                .map(|a| a.port())
                .unwrap_or(0);

            app.manage(AppState {
                db: TrackedDb::new(conn),
                db_read: Mutex::new(db::open_read(&db_file).expect("db open (lectura)")),
                player: Player::new(),
                covers: Mutex::new(HashMap::new()),
                music_dir: music_dir.clone(),
                peers: discovery::Peers::default(),
                mdns: Mutex::new(None),
                pairing: pairing::Pairing::default(),
                expected_paths: Mutex::new(HashMap::new()),
                autosync: autosync::AutoSync::default(),
            });

            if let Some(listener) = listener {
                pairing::spawn_server(app.handle().clone(), listener);
                discovery::spawn_prober(app.handle().clone());
                autosync::spawn(app.handle().clone());
            }

            // Descubrimiento. Va después de `manage` porque el thread de mDNS
            // resuelve el estado por el AppHandle.
            if sync_port != 0 {
                let state = app.state::<AppState>();
                let ident = {
                    let conn = state.db.lock().unwrap();
                    db::this_device_uid(&conn)
                        .and_then(|uid| db::device_name(&conn).map(|name| (uid, name)))
                };
                match ident {
                    Ok((uid, name)) => {
                        match discovery::start(app.handle().clone(), &uid, &name, sync_port) {
                            Ok(daemon) => *state.mdns.lock().unwrap() = Some(daemon),
                            Err(e) => eprintln!("[mdns] no se pudo iniciar: {e}"),
                        }
                    }
                    Err(e) => eprintln!("[mdns] sin identidad de dispositivo: {e}"),
                }
            } else {
                eprintln!("[mdns] no se pudo reservar puerto, descubrimiento apagado");
            }

            // Watch de la carpeta gestionada: archivos nuevos (copiados por el
            // usuario fuera de la app, o por otro medio) se importan solos.
            // Lo que cumplió la retención se va de verdad. Al arrancar y no
            // en un timer: es una limpieza, no algo urgente.
            trash::purge_old(&music_dir, trash::RETENTION_DAYS);

            spawn_folder_watch(app.handle().clone(), music_dir);
            // Despues del watch: el backfill puede tardar minutos con una
            // biblioteca grande y no debe demorar el arranque.
            spawn_hash_backfill(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            import_folder,
            import_files,
            import_from_uri,
            track_playlists,
            list_tracks,
            delete_tracks,
            set_volume,
            cover_thumb,
            reveal_track,
            get_track_path,
            list_playlists,
            create_playlist,
            rename_playlist,
            delete_playlist,
            move_playlist,
            playlist_tracks,
            add_tracks_to_playlist,
            remove_tracks_from_playlist,
            reorder_playlist_tracks,
            play_track,
            pause_playback,
            resume_playback,
            stop_playback,
            seek_to,
            playback_position,
            export_library_xml_now,
            get_auto_sync_xml,
            set_auto_sync_xml,
            sync_xml_after_change,
            get_auto_sync_p2p,
            set_auto_sync_p2p,
            device_identity,
            set_device_name,
            list_peers,
            refresh_peers,
            connect_peer,
            preview_sync,
            sync_files,
            confirm_pairing,
            unpair_device,
            get_delete_policy,
            set_delete_policy,
            get_scope,
            set_scope_mode,
            set_scope_direction,
            set_scope_playlist,
            set_scope_playlists,
            storage_status,
            free_space,
            list_pending_deletes,
            resolve_pending_delete,
            sync_history
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
