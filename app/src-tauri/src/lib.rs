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

pub struct AppState {
    db: Mutex<Connection>,
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

#[tauri::command]
fn list_tracks(state: State<AppState>) -> Result<Vec<db::Track>, String> {
    let conn = state.db.lock().unwrap();
    db::list_tracks(&conn).map_err(|e| e.to_string())
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
    let conn = state.db.lock().unwrap();
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
    let conn = state.db.lock().unwrap();
    db::list_playlists(&conn).map_err(|e| e.to_string())
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
        db::create_playlist(&conn, &name, &kind, parent_id).map_err(|e| e.to_string())?
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
    let conn = state.db.lock().unwrap();
    db::playlist_tracks(&conn, playlist_id).map_err(|e| e.to_string())
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
            eprintln!("[db] archivo: {}", db_file.display());
            let conn = db::open(&db_file).expect("db open");
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
                db: Mutex::new(conn),
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
            unpair_device
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
