mod cover;
mod db;
mod export_xml;
mod import;
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

pub struct AppState {
    db: Mutex<Connection>,
    player: Player,
    covers: Mutex<HashMap<i64, Option<String>>>,
    /// Carpeta gestionada (<Música>/Sway): todo lo importado se copia acá.
    music_dir: PathBuf,
}

#[tauri::command]
fn import_folder(app: AppHandle, state: State<AppState>, folder: String) -> Result<usize, String> {
    let conn = state.db.lock().unwrap();
    import::import_folder(&conn, &state.music_dir, &folder, |done, total| {
        let _ = app.emit("import-progress", (done, total));
    })
    .map_err(|e| e.to_string())
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
    let conn = state.db.lock().unwrap();
    import::import_paths(&conn, &state.music_dir, &paths, |done, total| {
        let _ = app.emit("import-progress", (done, total));
    })
    .map_err(|e| e.to_string())
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
    let conn = state.db.lock().unwrap();
    import::import_bytes(&conn, &state.music_dir, &resolved_name, &bytes).map_err(|e| e.to_string())
}

#[tauri::command]
fn track_playlists(state: State<AppState>, id: i64) -> Result<Vec<i64>, String> {
    let conn = state.db.lock().unwrap();
    db::track_playlists(&conn, id).map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_tracks(state: State<AppState>, ids: Vec<i64>) -> Result<(), String> {
    let music = state.music_dir.clone();
    let mut conn = state.db.lock().unwrap();
    db::delete_tracks(&mut conn, &music, &ids).map_err(|e| e.to_string())
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
    state: State<AppState>,
    name: String,
    kind: String,
    parent_id: Option<i64>,
) -> Result<i64, String> {
    let conn = state.db.lock().unwrap();
    db::create_playlist(&conn, &name, &kind, parent_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn rename_playlist(state: State<AppState>, id: i64, name: String) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    db::rename_playlist(&conn, id, &name).map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_playlist(state: State<AppState>, id: i64) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    db::delete_playlist(&conn, id).map_err(|e| e.to_string())
}

#[tauri::command]
fn move_playlist(
    state: State<AppState>,
    id: i64,
    parent_id: Option<i64>,
    index: i64,
) -> Result<(), String> {
    let mut conn = state.db.lock().unwrap();
    db::move_playlist(&mut conn, id, parent_id, index)
}

#[tauri::command]
fn playlist_tracks(state: State<AppState>, playlist_id: i64) -> Result<Vec<db::Track>, String> {
    let conn = state.db.lock().unwrap();
    db::playlist_tracks(&conn, playlist_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn add_tracks_to_playlist(
    state: State<AppState>,
    playlist_id: i64,
    track_ids: Vec<i64>,
) -> Result<usize, String> {
    let mut conn = state.db.lock().unwrap();
    db::add_tracks_to_playlist(&mut conn, playlist_id, &track_ids).map_err(|e| e.to_string())
}

#[tauri::command]
fn remove_tracks_from_playlist(
    state: State<AppState>,
    playlist_id: i64,
    track_ids: Vec<i64>,
) -> Result<(), String> {
    let mut conn = state.db.lock().unwrap();
    db::remove_tracks_from_playlist(&mut conn, playlist_id, &track_ids).map_err(|e| e.to_string())
}

#[tauri::command]
fn reorder_playlist_tracks(
    state: State<AppState>,
    playlist_id: i64,
    track_ids: Vec<i64>,
    index: i64,
) -> Result<(), String> {
    let mut conn = state.db.lock().unwrap();
    db::reorder_playlist_tracks(&mut conn, playlist_id, &track_ids, index).map_err(|e| e.to_string())
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
#[tauri::command]
fn sync_xml_after_change(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    xml_sync::write_if_enabled(&app, &conn, &state.music_dir);
    Ok(())
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
            let paths: Vec<String> = events
                .into_iter()
                .map(|e| e.path.to_string_lossy().into_owned())
                .collect();
            if paths.is_empty() {
                continue;
            }
            let state = handle.state::<AppState>();
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
                let _ = handle.emit("library-changed", ());
            }
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
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
            // Carpeta gestionada: <Música del usuario>/Sway (fallback: appdata).
            let music_dir = app
                .path()
                .audio_dir()
                .unwrap_or_else(|_| dir.clone())
                .join("Sway");
            std::fs::create_dir_all(&music_dir).ok();
            eprintln!("[lib] carpeta gestionada: {}", music_dir.display());
            app.manage(AppState {
                db: Mutex::new(conn),
                player: Player::new(),
                covers: Mutex::new(HashMap::new()),
                music_dir: music_dir.clone(),
            });

            // Watch de la carpeta gestionada: archivos nuevos (copiados por el
            // usuario fuera de la app, o por otro medio) se importan solos.
            spawn_folder_watch(app.handle().clone(), music_dir);
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
            sync_xml_after_change
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
