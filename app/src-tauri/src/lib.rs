mod cover;
mod db;
mod import;
mod player;

use player::Player;
use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::Mutex;
use tauri::{Manager, State};

pub struct AppState {
    db: Mutex<Connection>,
    player: Player,
    covers: Mutex<HashMap<i64, Option<String>>>,
}

#[tauri::command]
fn import_folder(state: State<AppState>, folder: String) -> Result<usize, String> {
    let conn = state.db.lock().unwrap();
    import::import_folder(&conn, &folder).map_err(|e| e.to_string())
}

#[tauri::command]
fn list_tracks(state: State<AppState>) -> Result<Vec<db::Track>, String> {
    let conn = state.db.lock().unwrap();
    db::list_tracks(&conn).map_err(|e| e.to_string())
}

#[tauri::command]
fn import_files(state: State<AppState>, paths: Vec<String>) -> Result<Vec<i64>, String> {
    let conn = state.db.lock().unwrap();
    import::import_paths(&conn, &paths).map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_tracks(state: State<AppState>, ids: Vec<i64>) -> Result<(), String> {
    let mut conn = state.db.lock().unwrap();
    db::delete_tracks(&mut conn, &ids).map_err(|e| e.to_string())
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
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
            app.manage(AppState {
                db: Mutex::new(conn),
                player: Player::new(),
                covers: Mutex::new(HashMap::new()),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            import_folder,
            import_files,
            list_tracks,
            delete_tracks,
            set_volume,
            cover_thumb,
            reveal_track,
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
            playback_position
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
