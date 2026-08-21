mod autosync;
mod cover;
mod discovery;
mod export_xml;
mod pairing;
mod power;
mod watch;
mod xml_sync;

// The engine has lived in `crates/core` since Phase 6.0 — the app and the
// headless server are two fronts over the same code. It's re-exported under
// the same names it had when it was all one crate: that way the
// `crate::db::…` calls in the modules that stayed here still resolve the
// same, and the extraction didn't have to touch the body of any file.
pub use sway_core::{
    db, device_info, engine, hashing, id3_sanitize, import, manifest, merge, perf_line, rank, scope,
    transfer, trash, wire,
};

// Desktop: real Player (rodio/symphonia, its own thread). Android/iOS: a
// stub with the same API — playback there goes through the native plugin
// from JS (see player_stub.rs and app/src/nativeAudio.ts); this module only
// exists so AppState and the playback commands compile the same.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod player;
#[cfg(any(target_os = "android", target_os = "ios"))]
#[path = "player_stub.rs"]
mod player;

// Measuring loudness and the silent edges of a file. Decoding is symphonia
// directly, not through rodio: rodio is the audio *output* and is desktop
// only, but the phone needs the same numbers — without them there is nothing
// for normalization to aim at and no silence for gapless to trim.
mod loudness;

use player::{Cue, Player};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

/// One-time migration: on Android tracks used to live in
/// `<managed folder>/Sway`, one level too deep — the managed folder is
/// already private to the app (`…/files/Music`), so that subdirectory
/// wasn't separating anything. Whatever was inside gets moved up one level.
///
/// The stored paths are absolute: if they're not updated along with the
/// files, every track ends up pointing at something that's no longer there.
/// Never deletes anything — on a name clash it disambiguates, the same as
/// import does.
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
        eprintln!("[lib] migrated: {} -> {}", from.display(), to.display());
    }
    // Only delete the directory if it ended up empty.
    std::fs::remove_dir(&legacy).ok();
    Ok(())
}

#[cfg(not(target_os = "android"))]
fn flatten_legacy_subdir(_conn: &Connection, _music_dir: &std::path::Path) -> anyhow::Result<()> {
    Ok(())
}

/// TEMPORARY — diagnostics.
///
/// Measuring the WAIT has already been done and it gave ~700 ms with the
/// write and commit at 0 ms, meaning the cost isn't in who writes but in who
/// was holding the lock. This wraps the mutex so every long hold logs itself
/// with the file and line of whoever took it — without having to hand-label
/// the twenty-odd places that use it.
pub struct TrackedDb(Mutex<Connection>);

#[derive(Debug)]
pub struct Poisoned;

impl TrackedDb {
    fn new(c: Connection) -> Self {
        Self(Mutex::new(c))
    }
    /// Same shape as `Mutex::lock` so callers don't have to change.
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
                "LOCK held {held} ms by {}:{}",
                self.caller.file(),
                self.caller.line()
            ));
        }
    }
}

pub struct AppState {
    db: TrackedDb,
    /// Read-only, for whatever draws the screen. See `db::open_read`: this
    /// is what keeps opening a playlist from waiting on the sync to release
    /// the lock.
    db_read: Mutex<Connection>,
    player: Player,
    covers: Mutex<HashMap<i64, Option<String>>>,
    /// Managed folder (<Music>/Sway): everything imported is copied here.
    music_dir: PathBuf,
    /// Sway devices seen on the local network (Phase 5.1).
    peers: discovery::Peers,
    /// Keeping the daemon alive is what keeps the service published.
    mdns: Mutex<Option<mdns_sd::ServiceDaemon>>,
    /// Pairing confirmations waiting on the user to look at the screen.
    pairing: pairing::Pairing,
    /// Files the sync just dropped into the managed folder.
    ///
    /// The watcher observes that folder and auto-imports whatever shows
    /// up — which is exactly what must NOT happen with something brought in
    /// by sync: it would create a row with a new uid (breaking the shared
    /// identity between devices) and would overwrite the synced metadata
    /// with the file's own tags. The sync notes the destination before the
    /// rename and the watcher skips it once.
    /// Keyed with a timestamp and not just one-shot: the filesystem emits
    /// several events per file (create, modify, close), so consuming the
    /// mark on the first one let the following ones through — and every
    /// re-import bumped the track's date, leaving this device "newer" on
    /// every round.
    expected_paths: Mutex<HashMap<PathBuf, i64>>,
    /// Auto-sync state (changes pending propagation).
    autosync: autosync::AutoSync,
    /// Network and battery (Phase 6.7). On desktop these are read here; on
    /// Android the screen reports them, since it's the only thing that can.
    conditions: Mutex<power::Conditions>,
    /// Open connections waiting for the server to announce changes.
    watchers: watch::Watchers,
    /// Background EBU R128 sweep behind "normalize volume" and the silent
    /// edges gapless trims to.
    loudness: loudness::Analyzer,
}

/// How long events are ignored for a file the sync just dropped.
/// Covers the burst of filesystem events without hiding a real change made
/// much later.
const EXPECTED_PATH_TTL_MS: i64 = 60_000;

impl AppState {
    pub fn expect_path(&self, p: &std::path::Path) {
        let now = db::now_ms();
        let mut map = self.expected_paths.lock().unwrap();
        map.retain(|_, ts| now - *ts < EXPECTED_PATH_TTL_MS);
        map.insert(p.to_path_buf(), now);
    }

    /// True if the path was written by the sync recently.
    fn is_expected(&self, p: &std::path::Path) -> bool {
        let now = db::now_ms();
        match self.expected_paths.lock().unwrap().get(p) {
            Some(ts) => now - *ts < EXPECTED_PATH_TTL_MS,
            None => false,
        }
    }
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncProgressEvent {
    uid: String,
    /// Index of the current file and total files for this run.
    file_index: usize,
    file_total: usize,
    filename: String,
    /// Bytes of the current file.
    done: u64,
    total: u64,
    sending: bool,
}

/// The sync engine running inside the app: the base is `AppState`'s, and
/// notifications go out as window events. The other implementation of
/// `Host` lives in the integrity suite (`engine.rs`), which runs this same
/// engine against a temp directory.
///
/// It's a wrapper and not a direct `impl` on `AppHandle` because since
/// Phase 6.0 the trait lives in another crate: you can't implement a
/// foreign trait for a foreign type. It wraps a borrow, so it costs nothing.
pub struct AppHost<'a>(pub &'a AppHandle);

impl engine::Host for AppHost<'_> {
    fn with_db<T>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let state = self.0.state::<AppState>();
        let conn = state
            .db
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned db lock"))?;
        f(&conn)
    }

    /// Whatever almost always returns zero can't be stuck behind a sync: see
    /// `db::open_read`.
    fn with_db_read<T>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let state = self.0.state::<AppState>();
        let conn = state
            .db_read
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned db lock"))?;
        f(&conn)
    }

    fn music_dir(&self) -> PathBuf {
        self.0.state::<AppState>().music_dir.clone()
    }

    fn expect_path(&self, dest: &std::path::Path) {
        self.0.state::<AppState>().expect_path(dest);
    }

    fn progress(&self, p: &engine::Progress) {
        let _ = self.0.emit(
            "sync-progress",
            SyncProgressEvent {
                uid: p.peer_uid.to_string(),
                file_index: p.index,
                file_total: p.total_files,
                filename: p.filename.to_string(),
                done: p.done,
                total: p.total,
                sending: p.sending,
            },
        );
    }

    fn library_changed(&self, force: bool) {
        pairing::emit_library_changed(self.0, force);
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
    kick_loudness(&app);
    Ok(n)
}

/// Wakes the analyzer after tracks arrive, so a track is measured once when
/// it enters the library rather than the first time something needs it.
///
/// Measuring is not gated on normalization being on. A track gets analyzed
/// when it shows up, so that turning normalization on is instant instead of
/// kicking off a decode of the whole library, and so the same pass can carry
/// whatever else gets measured later. It runs on one background thread and
/// touches each file once, ever.
///
/// Cheap to call from anywhere: the analyzer ignores it when a sweep is
/// already running — that sweep re-reads what's pending on every batch and
/// will reach the new rows on its own.
pub fn kick_loudness(app: &AppHandle) {
    app.state::<AppState>().loudness.sweep(app.clone());
}

/// Reports when something running with the DB lock held took long enough
/// to be noticeable on screen. Without this, "it freezes for a second" is
/// impossible to attribute: every refresh runs three separate queries and
/// any one of the three could be the culprit.
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

#[tauri::command]
fn list_tracks(state: State<AppState>) -> Result<Vec<db::Track>, String> {
    let conn = state.db_read.lock().unwrap();
    timed("list_tracks", || {
        let mut tracks = db::list_tracks(&conn).map_err(|e| e.to_string())?;
        timed("  mark_scope", || mark_scope(&conn, &mut tracks));
        Ok(tracks)
    })
}

/// Marks which tracks are part of what this device syncs. The table shows
/// them dimmed: without this, unmarking a playlist wouldn't be noticeable
/// anywhere until someone freed up space.
fn mark_scope(conn: &Connection, tracks: &mut [db::Track]) {
    let me = match db::this_device_uid(conn) {
        Ok(v) => v,
        Err(_) => return,
    };
    let Ok(Some(in_scope)) = scope::scope_tracks(conn, &me) else {
        return; // scope = everything (or error): keep the default, everything in
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
    kick_loudness(&app);
    Ok(ids)
}

/// Import from a file picker (Android/iOS): the picker gives a `content://`
/// (or `file://`) URI, not a raw filesystem path that `std::fs` can open
/// directly. `tauri_plugin_fs`'s `Fs::read` knows how to resolve those URIs
/// (explicit Android support in the crate), so the read goes through there
/// instead of through std::fs.
#[tauri::command]
fn import_from_uri(app: AppHandle, state: State<AppState>, uri: String, name: String) -> Result<i64, String> {
    use std::str::FromStr;
    use tauri_plugin_fs::{FilePath, FsExt};
    // app.path().file_name() resolves the real name via Android's native
    // API for content:// (Tauri documents this explicitly). Much more
    // reliable than guessing from the URI's text on the frontend side —
    // `name` (whatever the picker sent) is only a fallback.
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

/// Resolves id -> file path. On desktop `play_track` already does this
/// internally in Rust; on Android JS needs the path to hand it to the
/// native plugin (`setSource`), so it needs to be exposed as a command.
#[tauri::command]
fn get_track_path(state: State<AppState>, id: i64) -> Result<String, String> {
    let conn = state.db.lock().unwrap();
    db::track_path(&conn, id).map_err(|e| e.to_string())
}

/// The same answer `cue_for` gives the desktop player, in the shape the
/// Android plugin takes.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TrackCue {
    path: String,
    /// Trim and normalization folded into one figure, in dB. Desktop turns
    /// this into a linear multiplier inside `cue_for`; the plugin wants the
    /// dB, so the conversion doesn't happen here.
    gain_db: f64,
    /// Where the audio actually starts and ends, in ms, as measured by the
    /// analyzer. This is what makes a transition gapless: the silence a file
    /// carries at its edges is the gap, and playing from `lead_ms` to
    /// `audio_end_ms` is how it stops being one.
    ///
    /// `audio_end_ms` is 0 when the track hasn't been measured yet, meaning
    /// "play to the end" — an unmeasured track plays untrimmed rather than
    /// having its edges guessed.
    lead_ms: i64,
    audio_end_ms: i64,
}

/// Path and level for a track, for the platforms whose player lives outside
/// Rust.
///
/// Android's audio is the native plugin, which JS drives, so the numbers the
/// desktop `Player` reads straight out of `Cue` have to make the trip out to
/// JS instead. Same inputs and the same `db::playback_gain_db` as `cue_for`,
/// so a track plays at one level on both platforms.
#[tauri::command]
fn track_cue(state: State<AppState>, id: i64) -> Result<TrackCue, String> {
    let conn = state.db.lock().unwrap();
    let path = db::track_path(&conn, id).map_err(|e| e.to_string())?;
    let a = db::track_playback(&conn, id).unwrap_or_default();
    let prefs = db::get_playback_prefs(&conn).unwrap_or_default();
    // Crossfade overlaps the tracks, so there is no gap for trimming to close
    // and cutting the tails would only shorten the overlap. Same rule the
    // desktop player follows.
    let trim = prefs.gapless && prefs.crossfade_secs <= 0.0;
    Ok(TrackCue {
        path,
        gain_db: db::playback_gain_db(a.gain_db, a.loudness_lufs, prefs.normalize),
        lead_ms: if trim { a.lead_silence_ms.unwrap_or(0).max(0) } else { 0 },
        audio_end_ms: if trim { a.audio_end_ms.unwrap_or(0).max(0) } else { 0 },
    })
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

/// Marks which playlists are part of what this device syncs. The main
/// view's tree greys out the ones that aren't, and hides the ones that were
/// also already freed; the scope editor shows all of them regardless, or
/// there'd be no way to mark them again.
fn mark_playlist_scope(conn: &Connection, nodes: &mut [db::PlaylistNode]) {
    let Ok(me) = db::this_device_uid(conn) else { return };
    let Ok(Some(selected)) = scope::scope_playlists(conn, &me) else {
        return; // scope = everything (or error): keep the default, everything in
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
        // The user created it right here: it shouldn't vanish instantly for
        // not being marked. Don't fail the command if this doesn't work —
        // the playlist already exists.
        if let Err(e) = scope::select_new_local(&conn, id) {
            log::warn!("[scope] could not select the new playlist: {e}");
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
        // Scoped to this playlist: marking twenty rows can't cost the same
        // as walking the whole library.
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

/// Everything the player needs about a track, resolved in one place: where
/// the file is, how loud to play it, and how long it runs.
///
/// The gain is worked out here rather than in the player because it depends
/// on the library (the track's trim, its measured loudness) and on a
/// preference — none of which the audio thread should be reaching for while
/// it's mid-decode.
fn cue_for(conn: &Connection, id: i64, normalize: bool) -> Result<Cue, String> {
    let (path, duration_ms): (String, i64) = conn
        .query_row("SELECT path, duration_ms FROM tracks WHERE id = ?1", [id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .map_err(|e| e.to_string())?;
    let a = db::track_playback(conn, id).unwrap_or_default();
    Ok(Cue {
        id,
        path: std::path::PathBuf::from(path),
        gain: db::db_to_linear(db::playback_gain_db(a.gain_db, a.loudness_lufs, normalize)),
        duration_ms: duration_ms.max(0) as u64,
        // Unanalyzed tracks trim nothing: playing them untrimmed is the old
        // behaviour, and guessing where the audio starts without measuring is
        // how you clip the first beat off a track.
        lead_ms: a.lead_silence_ms.unwrap_or(0).max(0) as u64,
        audio_end_ms: a.audio_end_ms.unwrap_or(0).max(0) as u64,
    })
}

#[tauri::command]
fn play_track(state: State<AppState>, id: i64) -> Result<(), String> {
    let cue = {
        let conn = state.db.lock().unwrap();
        let normalize = db::get_playback_prefs(&conn).unwrap_or_default().normalize;
        cue_for(&conn, id, normalize)?
    };
    state.player.play(cue);
    Ok(())
}

/// What to play when the current track ends. The UI owns the queue, so it is
/// the only thing that knows this; the player owns the *transition*, so it is
/// the only thing that can make it gapless. `None` = nothing follows.
///
/// Sent on every track change rather than only when a mode needs it: the
/// player ignores it unless gapless or crossfade is on, and this way turning
/// crossfade on mid-track takes effect on the very next boundary.
#[tauri::command]
fn set_next_track(state: State<AppState>, id: Option<i64>) -> Result<(), String> {
    let next = match id {
        None => None,
        Some(id) => {
            let conn = state.db.lock().unwrap();
            let normalize = db::get_playback_prefs(&conn).unwrap_or_default().normalize;
            Some(cue_for(&conn, id, normalize)?)
        }
    };
    state.player.set_next(next);
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

/// Position **and** which track is playing. The second half is what matters:
/// with gapless and crossfade the player moves to the next track by itself,
/// so the position alone no longer tells the UI what it's showing. Polled on
/// the same interval the position always was — no new machinery.
#[tauri::command]
fn playback_state(state: State<AppState>) -> player::PlaybackState {
    state.player.state()
}

// ---------------------------------------------------------------------------
// Playback preferences and levels
// ---------------------------------------------------------------------------

#[tauri::command]
fn get_playback_prefs(state: State<AppState>) -> Result<db::PlaybackPrefs, String> {
    let conn = state.db_read.lock().unwrap();
    db::get_playback_prefs(&conn).map_err(|e| e.to_string())
}

/// Saves the preferences and pushes the ones the audio thread cares about.
///
/// Turning normalization on or off changes how loud the *current* track
/// should be, so its gain is recomputed right here — otherwise the switch
/// would appear to do nothing until the next track.
#[tauri::command]
fn set_playback_prefs(
    app: AppHandle,
    state: State<AppState>,
    prefs: db::PlaybackPrefs,
) -> Result<(), String> {
    {
        let conn = state.db.lock().unwrap();
        db::set_playback_prefs(&conn, &prefs).map_err(|e| e.to_string())?;
        if let Some(id) = state.player.state().track_id {
            if let Ok(cue) = cue_for(&conn, id, prefs.normalize) {
                state.player.set_gain(cue.gain);
            }
        }
    }
    state.player.configure(prefs.crossfade_secs as f32, prefs.gapless);
    state.player.set_device(prefs.output_device.clone());
    // Turning normalization on with a partly-measured library: make sure the
    // sweep is awake so the gap closes instead of sitting there.
    if prefs.normalize {
        kick_loudness(&app);
    }
    Ok(())
}

#[tauri::command]
fn list_output_devices() -> Vec<String> {
    player::output_devices()
}

/// Per-track trim, in dB. Saved on the track: a quiet record is quiet every
/// time it comes up, so correcting it once should be the last time.
#[tauri::command]
fn set_track_gain(state: State<AppState>, id: i64, gain_db: f64) -> Result<(), String> {
    let clamped = gain_db.clamp(-db::MAX_GAIN_DB, db::MAX_GAIN_DB);
    let conn = state.db.lock().unwrap();
    db::set_track_gain(&conn, id, clamped).map_err(|e| e.to_string())?;
    // Applied live only when it's this track that's playing — the knob is on
    // the player bar, so that's the usual case, but the table can set it on
    // any row.
    if state.player.state().track_id == Some(id) {
        let normalize = db::get_playback_prefs(&conn).unwrap_or_default().normalize;
        if let Ok(cue) = cue_for(&conn, id, normalize) {
            state.player.set_gain(cue.gain);
        }
    }
    Ok(())
}

/// How many tracks are still waiting to be analyzed. 0 = the library is fully
/// measured.
#[tauri::command]
fn loudness_pending(state: State<AppState>) -> Result<i64, String> {
    let conn = state.db_read.lock().unwrap();
    db::analysis_pending_count(&conn).map_err(|e| e.to_string())
}

/// Throws away every measurement and starts over. What the Rescan button
/// calls — the analyzer only ever looks at rows it hasn't measured, so
/// clearing the results is how you ask for them again.
#[tauri::command]
fn rescan_analysis(app: AppHandle, state: State<AppState>) -> Result<i64, String> {
    {
        let conn = state.db.lock().unwrap();
        db::clear_analysis(&conn).map_err(|e| e.to_string())?;
    }
    kick_loudness(&app);
    let conn = state.db_read.lock().unwrap();
    db::analysis_pending_count(&conn).map_err(|e| e.to_string())
}

/// Identity of this device, to display it and let it be renamed in
/// Settings. The uid can't be changed (tombstones and clocks reference it);
/// the name is only so it can be recognized in the other side's device
/// list.
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

/// Sway devices visible on the local network. The frontend calls this when
/// opening the sync section and every time the `peers-changed` event
/// arrives.
#[tauri::command]
fn list_peers(state: State<AppState>) -> Vec<discovery::Peer> {
    let conn = state.db.lock().unwrap();
    state.peers.merged_list(&conn)
}

/// The "Refresh" button: queries the network again (mDNS) and immediately
/// rechecks who's reachable, without waiting for the periodic probe.
/// Re-reading the local list wasn't enough — it was already up to date;
/// what was missing was asking outward.
///
/// mDNS responses arrive via the `peers-changed` event, not in the return
/// value: the protocol is asynchronous and peers answer when they answer.
/// The probe itself is immediate.
///
/// Note: this does NOT change who's paired. Pairing lives in each device's
/// `devices` table and is synced by announcing (see `pairing::unpair`), not
/// by watching the network.
#[tauri::command]
fn refresh_peers(app: AppHandle) -> Result<(), String> {
    discovery::refresh(&app).map_err(|e| e.to_string())?;
    let handle = app.clone();
    std::thread::spawn(move || discovery::probe_once(&handle));
    Ok(())
}

/// Pairs with a peer, or if already paired asks it for its library counts.
/// Returns right away: the result arrives via the `pairing-request` /
/// `pairing-done` / `peer-hello` events, because on the other side there
/// might be a person taking their time to confirm.
#[tauri::command]
fn connect_peer(app: AppHandle, uid: String) {
    pairing::connect_peer(app, uid);
}

/// Sync dry run: compares the two libraries and publishes what would happen
/// via the `sync-plan` event. Writes nothing — the real transfer is 5.4.
#[tauri::command]
fn preview_sync(app: AppHandle, uid: String) {
    pairing::preview_sync(app, uid);
}

/// Runs the plan's file transfer. Files only: metadata, playlists, and
/// deletions arrive in 5.5/5.6. Progress goes out via `sync-progress` and
/// the final summary via `sync-done`.
#[tauri::command]
fn sync_files(app: AppHandle, uid: String) {
    pairing::sync_files(app, uid);
}

/// The user's response to the verification code.
#[tauri::command]
fn confirm_pairing(app: AppHandle, uid: String, accept: bool) -> bool {
    pairing::resolve_decision(&app, &uid, accept)
}

// ---------------------------------------------------------------------------
// Metered network and battery (Phase 6.7)
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncConditions {
    /// Whatever can be measured right now. Every field can be `null`: not
    /// knowing is a different answer than knowing it's not the case.
    now: power::Conditions,
    limits: power::Limits,
}

/// Network and battery state, plus the configured limits. The screen uses
/// these to avoid offering an option that means nothing on this device — a
/// desktop PC has no battery, and asking about it would be noise.
#[tauri::command]
fn sync_conditions(state: State<AppState>) -> Result<SyncConditions, String> {
    let now = power::current(&state);
    let conn = state.db.lock().map_err(|_| "db lock".to_string())?;
    Ok(SyncConditions { now, limits: power::Limits::load(&conn) })
}

#[tauri::command]
fn set_sync_limits(state: State<AppState>, on_metered: bool, min_battery: u8) -> Result<(), String> {
    let limits = power::Limits { on_metered, min_battery: min_battery.min(100) };
    let conn = state.db.lock().map_err(|_| "db lock".to_string())?;
    limits.save(&conn).map_err(|e| e.to_string())
}

/// What the screen can measure and Rust can't.
///
/// On Android, network and battery state can only be read from Java, and
/// the JNI context isn't initialized on the Rust side (see
/// `device_info.rs`). The webview, on the other hand, has
/// `navigator.getBattery()` and `navigator.connection`, so it reports it
/// from there.
#[tauri::command]
fn report_conditions(
    state: State<AppState>,
    metered: Option<bool>,
    battery_pct: Option<u8>,
    charging: Option<bool>,
) {
    let mut c = match state.conditions.lock() {
        Ok(c) => c,
        Err(_) => return,
    };
    *c = power::Conditions { metered, battery_pct, charging };
}

/// Pairs with a file server by address (Phase 6.3).
///
/// Unlike the rest of pairing, this answers within the same call: there's
/// no one on the other side who takes time to decide, so the UI doesn't
/// need to wait for an event. Returns the name the server declared.
#[tauri::command]
fn pair_with_server(app: AppHandle, host: String, port: u16, token: String) -> Result<String, String> {
    pairing::pair_with_server(&app, host.trim(), port, token.trim()).map_err(|e| e.to_string())
}

#[tauri::command]
fn unpair_device(app: AppHandle, uid: String) -> Result<(), String> {
    pairing::unpair(&app, &uid).map_err(|e| e.to_string())?;
    let _ = app.emit("peers-changed", ());
    Ok(())
}

// ---------------------------------------------------------------------------
// Selective scope and storage (Phase 5.7)
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ScopeView {
    mode: String,
    /// both | send | receive | off — what THAT device does.
    direction: String,
    /// Uids of manually marked playlists/folders. Whatever hangs off a
    /// marked folder is included without appearing here — the tree is
    /// resolved by the backend (see `scope::expand`), not the UI.
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

/// What a device does: sends, receives, both, or nothing. Editable from
/// either side — it's replicated data, same as scope.
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

/// Scope is replicated data: changing it here triggers a sync, so the
/// affected device finds out even if the change was made on the other one.
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

/// A scope change has two immediate effects: it has to be propagated (it's
/// replicated data) and, if the change is to THIS device's scope, whatever
/// just came back into scope has to be rescued from the trash — before the
/// next sync requests it over the network.
/// How long to wait, since the last scope change, before doing the
/// expensive part.
///
/// The row is written on the spot: it's microseconds and it's what keeps
/// the screen honest. What waits is the "after": reloading the whole
/// library over IPC, scanning the trash (which hashes), and waking up the
/// sync. Marking a folder writes one row per playlist inside it, and doing
/// that work per row meant several full reloads and several scans fighting
/// over the same SQLite lock — the screen would freeze on every tick.
///
/// Deliberately short, and exists only to coalesce a burst of clicks into
/// one refresh. It reached 400 ms back when every change dragged along
/// expensive work billed per row; that work is gone now — what runs now is
/// a couple of sub-millisecond queries — so lengthening the wait would only
/// delay the library refresh behind it without saving anything. Just
/// enough for two ticks in a row to coalesce into a single refresh.
const SCOPE_SETTLE_MS: u64 = 120;
/// How long the SYNC waits after a scope change. Much more than the
/// screen's refresh, and deliberately so.
///
/// `note_change` wakes up autosync, and a sync isn't cheap: it builds the
/// manifest with the lock held, talks over the network, applies the merge
/// with the lock held, and on finishing emits `library-changed`, which
/// forces the UI to reload everything. Measured on desktop it's around a
/// second; on the phone, more. Chained to every tick, that was the freeze:
/// not the queries, the sync.
///
/// Marking playlists happens in a burst — the panel opens, several get
/// ticked, it closes —, so waiting for the person to finish doesn't delay
/// anything noticeable. The only thing delayed is the other device finding
/// out, and nobody's watching for that.
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

    // Refresh the screen: short, it's the only thing the person is waiting on.
    if mine {
        let mark = SCOPE_GEN.fetch_add(1, Ordering::SeqCst) + 1;
        let app = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(SCOPE_SETTLE_MS));
            // Another click came in while sleeping: let the last one handle it, once.
            if SCOPE_GEN.load(Ordering::SeqCst) != mark {
                return;
            }
            // The rescue first: it takes the lock and may hash. Emitting
            // before this sent the UI off to request the whole library
            // right against that same lock, and the two waited on each
            // other.
            timed("restore_local", || engine::restore(&AppHost(&app)));
            // Marking or unmarking changes what's visible — which
            // playlists are in the tree, which tracks show up and which
            // stay dimmed — and that comes from `list_playlists` /
            // `list_tracks`, which the UI doesn't request again on its
            // own. Without this event the change wouldn't show up until
            // restart.
            let _ = app.emit("library-changed", ());
            kick_loudness(&app);
        });
    }

    // Propagate to the other device: long, nobody's waiting on it.
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

/// Several scope rows at once.
///
/// Marking a folder changes one row per playlist inside it. Sending them
/// one at a time was an IPC round trip, an implicit transaction (i.e. an
/// fsync), and an `after_scope_change` per row: with a large folder, the
/// tick took a while to release and the next click ended up waiting. Here
/// it's one trip, one commit, and a single after.
#[tauri::command]
fn set_scope_playlists(
    app: AppHandle,
    state: State<AppState>,
    device_uid: String,
    changes: Vec<ScopeChange>,
) -> Result<(), String> {
    {
        // TEMPORARY — diagnostics. This is the path the click triggers, and
        // the only one that still takes the write lock: worth separating
        // the wait from the commit.
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
                "set_scope_playlists: lock {lock_ms} ms, write {write_ms} ms, commit {commit_ms} ms, {} row(s)",
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
    /// Bytes of the files that are present on this device.
    library_bytes: i64,
    tracks_present: i64,
    /// Rows whose file is no longer here (evicted by selective sync).
    tracks_absent: i64,
    /// What can be freed right now with no risk: out of scope and with a
    /// confirmed copy on another paired device.
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

/// Frees up space: files out of scope go to the library trash (30 days) and
/// their rows become `absent`. Nothing is deleted from the library — the
/// tracks are still visible, greyed out, and re-marking their playlist
/// brings them back down.
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


#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LogEntry {
    ts: i64,
    kind: String,
    detail: String,
}

/// History of a device (pairing, syncs, conflicts).
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

/// Manual "Sync now": writes the XML regardless of the toggle's state.
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

/// Called by the frontend after every mutation (import, playlists, tracks).
/// The backend decides whether to do anything: if the toggle is off, no-op.
///
/// XML only: P2P sync finds out on its own, from the commands that mutate
/// the library. Notifying it from here too would fire it twice per
/// operation, since the frontend calls this AFTER every mutation.
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

/// Fills in `rel_path` (the file's name within the managed folder) on rows
/// that don't have it. `path` is absolute and therefore local:
/// `E:\...\Sway\x.flac` on the PC and `/storage/.../Music/x.flac` on the
/// phone. What travels between devices is `rel_path`; the absolute path is
/// rebuilt on each one. Legacy tracks from outside the managed folder are
/// left without `rel_path` — they aren't syncable until they're
/// re-imported.
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

/// Hashes, in the background, the tracks that don't have a `content_hash`
/// yet.
///
/// Runs on its own thread and **releases the DB lock between files**: with
/// a 100 GB library this takes minutes, and holding the mutex the whole
/// time would leave the UI frozen for that whole stretch. The hash itself
/// is computed outside the lock; only the initial SELECT and each row's
/// UPDATE go inside it.
///
/// Resumable by construction: what's "missing" is determined from the DB,
/// so closing the app halfway through just leaves whatever didn't get
/// hashed pending.
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
        eprintln!("[hash] {total} tracks without a hash");
        for (i, (id, path)) in pending.into_iter().enumerate() {
            let p = std::path::PathBuf::from(&path);
            let stamp = hashing::file_stamp(&p);
            let hash = match stamp {
                Ok(_) => hashing::hash_file(&p).ok(),
                Err(_) => None, // missing file: retried on the next startup
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
        eprintln!("[hash] backfill complete");
        let _ = handle.emit("hash-progress", (total, total));
    });
}

/// Watches the managed folder and auto-imports new audio files.
/// Runs on its own thread; emits `library-changed` when the count changes.
fn spawn_folder_watch(handle: AppHandle, dir: PathBuf) {
    use notify_debouncer_mini::new_debouncer;
    use notify_debouncer_mini::notify::RecursiveMode;
    use std::time::Duration;

    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut debouncer = match new_debouncer(Duration::from_millis(900), tx) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[watch] could not start: {e}");
                return;
            }
        };
        if let Err(e) = debouncer.watcher().watch(&dir, RecursiveMode::Recursive) {
            eprintln!("[watch] watch failed: {e}");
            return;
        }
        eprintln!("[watch] watching {}", dir.display());
        for res in rx {
            let events = match res {
                Ok(ev) => ev,
                Err(_) => continue,
            };
            let state = handle.state::<AppState>();
            let paths: Vec<String> = events
                .into_iter()
                // What the sync just dropped here is already in the DB
                // with the other device's uid: re-importing it would
                // duplicate it incorrectly.
                .filter(|e| !state.is_expected(&e.path))
                // And whatever is in .sway-trash / .sway-incoming isn't
                // library content: it's what was just deleted and what's
                // being downloaded. Auto-importing it would resurrect
                // every deletion.
                .filter(|e| !import::is_internal_path(&e.path))
                .map(|e| e.path.to_string_lossy().into_owned())
                .collect();
            if paths.is_empty() {
                continue;
            }
            // A file already in the library has nothing to import, and
            // checking that is one query per path.
            //
            // Without this, every watcher event went through the WRITE
            // lock and read the tags of everything in the batch. On
            // Android the managed folder lives on emulated storage and the
            // watcher fires nonstop on the same files: measured on device,
            // 200 to 1500 ms holds chained back to back, importing
            // nothing. Everything else — opening a playlist, marking a
            // scope — queued up behind that.
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
                eprintln!("[watch] imported new files ({} paths), notifying UI", paths.len());
                autosync::note_change(&handle);
                let _ = handle.emit("library-changed", ());
                kick_loudness(&handle);
            }
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Desktop: without this the sync's log::* calls aren't seen anywhere.
    // RUST_LOG=debug for more detail; info by default.
    //
    // The decoders are pinned to `error`. At their own default they narrate
    // every ID3 frame they don't implement, every VBR duration they estimate
    // and every stray RIFF chunk — hundreds of lines per track, none of them
    // actionable, and they bury the handful of lines that say what the app
    // itself is doing. That noise has already cost this project real
    // debugging time. `RUST_LOG=symphonia=info` brings them back when the
    // question actually is about decoding.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(
            "info,symphonia=error,symphonia_core=error,symphonia_bundle_mp3=error,\
             symphonia_metadata=error,symphonia_format_riff=error,symphonia_codec_pcm=error,\
             lofty=error",
        ))
        .try_init();
    }

    #[cfg(target_os = "android")]
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Debug)
            .with_tag("sway"),
    );

    #[allow(unused_mut)] // mut is only needed in the cfg below (android/ios)
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
            // TEMPORARY — see `perf_line`. Truncated on every startup so
            // what's read is always from the current run.
            let perf = dir.join("perf.log");
            std::fs::write(&perf, "").ok();
            sway_core::set_perf_file(perf);
            eprintln!("[db] file: {}", db_file.display());
            let conn = db::open(&db_file).expect("db open");
            // WAL checkpointing happens separately: see `db::open`, no
            // click ever pays for it.
            db::spawn_checkpointer(&db_file);
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
                .unwrap_or(-1);
            eprintln!("[db] tracks at startup: {n}");
            // Managed folder. On desktop `audio_dir()` is the user's Music
            // folder, shared with everything else, so Sway stays in its
            // own subdirectory. On Android it's already a folder private
            // to the app (`getExternalFilesDir(DIRECTORY_MUSIC)`, i.e.
            // `…/files/Music`): nesting another "Sway" inside it would
            // only add a level that doesn't separate anything.
            let audio_dir = app.path().audio_dir().unwrap_or_else(|_| dir.clone());
            let music_dir = if cfg!(target_os = "android") {
                audio_dir
            } else {
                audio_dir.join("Sway")
            };
            std::fs::create_dir_all(&music_dir).ok();
            if let Err(e) = flatten_legacy_subdir(&conn, &music_dir) {
                eprintln!("[lib] managed folder migration failed: {e}");
            }
            eprintln!("[lib] managed folder: {}", music_dir.display());
            match backfill_rel_paths(&conn, &music_dir) {
                Ok(n) if n > 0 => eprintln!("[lib] rel_path filled in on {n} tracks"),
                Err(e) => eprintln!("[lib] rel_path backfill failed: {e}"),
                _ => {}
            }
            // This device's identity: generated once and never changes
            // again (it signs tombstones and breaks LWW ties).
            match (db::this_device_uid(&conn), db::device_name(&conn)) {
                (Ok(uid), Ok(name)) => eprintln!("[sync] this device: {name} ({uid})"),
                (a, b) => eprintln!("[sync] could not establish identity: {a:?} / {b:?}"),
            }
            // Ephemeral port reserved by the OS: announced via mDNS and
            // where the sync server listens.
            let listener = std::net::TcpListener::bind("0.0.0.0:0").ok();
            let sync_port = listener
                .as_ref()
                .and_then(|l| l.local_addr().ok())
                .map(|a| a.port())
                .unwrap_or(0);

            app.manage(AppState {
                db: TrackedDb::new(conn),
                db_read: Mutex::new(db::open_read(&db_file).expect("db open (read)")),
                player: Player::new(),
                covers: Mutex::new(HashMap::new()),
                music_dir: music_dir.clone(),
                peers: discovery::Peers::default(),
                mdns: Mutex::new(None),
                pairing: pairing::Pairing::default(),
                expected_paths: Mutex::new(HashMap::new()),
                autosync: autosync::AutoSync::default(),
                conditions: Mutex::new(power::Conditions::default()),
                watchers: watch::Watchers::default(),
                loudness: loudness::Analyzer::default(),
            });

            // Devices with a fixed address (the file server) go before the
            // probe: nobody announces those, so if they're not restored
            // here they disappear on every startup.
            pairing::restore_manual_peers(app.handle());

            if let Some(listener) = listener {
                pairing::spawn_server(app.handle().clone(), listener);
                discovery::spawn_prober(app.handle().clone());
                autosync::spawn(app.handle().clone());
                watch::spawn(app.handle().clone());
            }

            // Discovery. Goes after `manage` because the mDNS thread
            // resolves state via the AppHandle.
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
                            Err(e) => eprintln!("[mdns] could not start: {e}"),
                        }
                    }
                    Err(e) => eprintln!("[mdns] no device identity: {e}"),
                }
            } else {
                eprintln!("[mdns] could not reserve a port, discovery disabled");
            }

            // Watch the managed folder: new files (copied into the app by
            // the user from outside, or by some other means) get imported
            // on their own. Whatever satisfied the retention period gets
            // truly deleted. On startup and not on a timer: it's cleanup,
            // not something urgent.
            trash::purge_old(&music_dir, trash::RETENTION_DAYS);

            spawn_folder_watch(app.handle().clone(), music_dir);
            // After the watch: the backfill can take minutes with a large
            // library and must not delay startup.
            spawn_hash_backfill(app.handle().clone());

            // Playback preferences are read once here and pushed to the audio
            // thread, so crossfade and the pinned output device survive a
            // restart without the UI having to re-send them on every startup.
            {
                let state = app.state::<AppState>();
                let prefs = {
                    let conn = state.db.lock().unwrap();
                    db::get_playback_prefs(&conn).unwrap_or_default()
                };
                state.player.configure(prefs.crossfade_secs as f32, prefs.gapless);
                state.player.set_device(prefs.output_device.clone());
            }
            // Picks up anything that entered the library while this device
            // was closed (imports on another machine, files brought by sync).
            kick_loudness(app.handle());
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
            track_cue,
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
            playback_state,
            set_next_track,
            get_playback_prefs,
            set_playback_prefs,
            list_output_devices,
            set_track_gain,
            loudness_pending,
            rescan_analysis,
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
            sync_conditions,
            set_sync_limits,
            report_conditions,
            pair_with_server,
            unpair_device,
            get_scope,
            set_scope_mode,
            set_scope_direction,
            set_scope_playlist,
            set_scope_playlists,
            storage_status,
            free_space,
            sync_history
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
