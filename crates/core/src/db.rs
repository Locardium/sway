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
    /// The file is on this device. `false` = the row is still here but the
    /// blob was evicted by selective sync (Phase 5.7), or hasn't been downloaded yet.
    pub present: bool,
    /// Falls within what this device syncs. `false` = its playlist isn't
    /// marked: the file that's already here isn't touched, but it won't be downloaded
    /// or updated. Filled in by the command, not the query — it depends on the
    /// scope, not the row.
    ///
    /// Together with `present` it decides how it's shown: out of scope with a file
    /// shows dimmed and doesn't play; out of scope without a file disappears from
    /// the main view (the space has already been freed).
    pub in_scope: bool,
    pub uid: Option<String>,
    /// Manual per-track gain in dB, the way a DJ mixer's trim works: a quiet
    /// track gets pushed up once and stays that way. Survives restarts, and is
    /// deliberately separate from the master volume (which is the room, not
    /// the track).
    pub gain_db: f64,
    /// Integrated loudness (EBU R128, LUFS) measured by the analyzer.
    /// `None` = not analyzed yet. Used only when "normalize volume" is on.
    pub loudness_lufs: Option<f64>,
}

const SCHEMA: &str = "
-- Identity of a track: TWO columns, not one.
--   `uid`          logical identity (UUID). Survives retagging, renaming, moving.
--                  It's what playlists, cues and tombstones reference, and the
--                  only thing that means anything on the other device (the `id`
--                  INTEGER is local: this machine's 42 isn't the other
--                  machine's 42).
--   `content_hash` identity of the bytes (blake3). It's what gets requested and
--                  verified during a transfer, and what allows deduplicating
--                  the same file imported twice separately.
-- Kept separate because editing the genre changes metadata but not bytes: same uid,
-- same hash, new clock. (Corollary: Sway does NOT rewrite tags inside the
-- files; if it ever did, the blob would need to be rehashed and propagated.)
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
    -- File name inside the managed folder. `path` is absolute and
    -- therefore local; this is what travels between devices.
    rel_path     TEXT,
    size_bytes   INTEGER,
    -- (size, mtime) are the hash cache: if they haven't changed, no rehash happens.
    mtime_ms     INTEGER,
    -- LWW per field: {\"artist\": [ts_ms, device_uid], ...}. Gets populated once
    -- metadata editing exists (Phase 5.5); currently stays NULL.
    field_clocks TEXT,
    updated_at   INTEGER NOT NULL DEFAULT 0,
    -- present  = the file is on this device
    -- absent   = the row is here, the blob isn't (selective sync: the phone
    --            knows the track but hasn't downloaded it)
    -- pending  = transfer in progress
    local_state  TEXT NOT NULL DEFAULT 'present',
    -- Manual trim in dB set from the player, like a mixer's gain knob.
    gain_db       REAL NOT NULL DEFAULT 0,
    -- Integrated loudness in LUFS (EBU R128), measured by the analyzer.
    -- NULL = never analyzed. Derived from the bytes, so it is NOT replicated:
    -- each device measures its own copy and gets the same number.
    -- It doubles as the has-this-been-analyzed marker: everything the
    -- analyzer measures is written in the same pass.
    loudness_lufs REAL,
    -- Where the audio actually starts and ends, as absolute positions in ms.
    -- This is what makes gapless gapless: appending one track after another
    -- inserts no gap, but the encoder padding and the recorded silence baked
    -- into the files still play, and that silence IS the gap you hear.
    --
    -- The end is stored absolute rather than as silence-from-the-end,
    -- because that would have to be subtracted from `duration_ms`, which for
    -- a VBR MP3 is a bitrate ESTIMATE and can be off by seconds. The analyzer
    -- decodes the file, so this position is exact.
    lead_silence_ms INTEGER,
    audio_end_ms    INTEGER
);
-- The uid/content_hash indexes are created by migrate(): on an old database
-- these columns don't exist yet when this batch runs.

-- Virtual hierarchy (folders + playlists) — used since Phase 2/3.
-- `rank` is a fractional rank (see rank.rs), not an index: reordering touches
-- a single row, so two devices reordering offline merge without
-- stomping on each other. The order is `ORDER BY rank` (BINARY collation).
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
    -- When it was added. Compared against `tombstones.deleted_at` for the same
    -- pair: without this, an old tombstone would win forever over a fresh add
    -- and re-adding a song to a playlist would undo itself on the
    -- next sync.
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

-- Persisted key/value config (e.g. auto-sync XML toggle).
CREATE TABLE IF NOT EXISTS app_settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- ---------------------------------------------------------------------------
-- Phase 5: P2P sync.
--
-- `devices` is LOCAL: it describes who THIS device syncs with.
--
-- `device_scope` and `sync_scope`, by contrast, ARE replicated (Phase 5.7): the
-- scope describes a desire (I want these playlists on the phone), not a
-- security rule, and can be edited from any device.
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS devices (
    uid          TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    platform     TEXT NOT NULL DEFAULT '',
    pubkey       BLOB,               -- static Noise key, fixed at pairing time
    paired_at    INTEGER,
    last_seen    INTEGER,
    last_sync_at INTEGER,            -- cutoff for the incremental manifest
    -- How up to date we are on THAT device's library (see
    -- `wire::Mark`). Deliberately survives the app closing: without this, every
    -- time Android kills the process the two whole libraries would have to be
    -- compared to discover that nothing changed — and on a phone that happens
    -- dozens of times a day.
    watch_epoch  INTEGER,
    watch_rev    INTEGER,
    -- `host:port` of a device that does NOT self-discover (Phase 6.3): the
    -- file server, which lives outside the LAN and therefore doesn't show up on
    -- mDNS. Devices on the local network leave this NULL — their address
    -- changes with DHCP, and the one that matters is the one they announce.
    address      TEXT
);

-- Sync preferences for EACH device (including this one). Replicated, LWW
-- by `updated_at`: they describe a property of the device (what it does, and which
-- playlists live there), and the same answer holds for everyone looking at it.
--
--   `direction`  what THAT device does: sends, receives, both, or neither.
--                Between two devices, A -> B happens only if A sends AND B
--                receives. Nothing needs to be negotiated over the network: both
--                sides read the same two rows.
--   `mode`       'all' or 'selected' (see sync_scope).
CREATE TABLE IF NOT EXISTS device_sync (
    device_uid TEXT PRIMARY KEY,
    mode       TEXT NOT NULL DEFAULT 'all',    -- all|selected
    direction  TEXT NOT NULL DEFAULT 'both',   -- both|send|receive|off
    updated_at INTEGER NOT NULL DEFAULT 0
);

-- Which playlists/folders each device wants. Replicated, LWW per row.
--
-- Unmarking does NOT delete the row: it leaves it with `selected = 0`. If the row were
-- deleted, the merge union would bring it back from the other side on the
-- next sync and unmarking would never stick.
CREATE TABLE IF NOT EXISTS sync_scope (
    device_uid   TEXT NOT NULL,
    playlist_uid TEXT NOT NULL,
    selected     INTEGER NOT NULL DEFAULT 1,
    updated_at   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (device_uid, playlist_uid)
);

-- Which devices have (or had) each blob. It's the only thing that allows
-- freeing up space without risking the last copy: a file out of scope
-- is only evicted if it's confirmed to live somewhere else.
CREATE TABLE IF NOT EXISTS blob_replicas (
    hash       TEXT NOT NULL,
    device_uid TEXT NOT NULL,
    seen_at    INTEGER NOT NULL,
    PRIMARY KEY (hash, device_uid)
);

-- A deletion without a tombstone is a deletion the next sync undoes: the other
-- device still has the row and sends it back to you, forever.
CREATE TABLE IF NOT EXISTS tombstones (
    entity     TEXT NOT NULL,   -- 'track'|'playlist'|'playlist_track'|'cue'
    uid        TEXT NOT NULL,   -- playlist_track: '<playlist_uid>:<track_uid>'
    deleted_at INTEGER NOT NULL,
    device_uid TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (entity, uid)
);

-- Readable history: dry-runs, automatically resolved conflicts (with the
-- losing value, so it can be reverted) and transfers.
CREATE TABLE IF NOT EXISTS sync_log (
    id     INTEGER PRIMARY KEY,
    ts     INTEGER NOT NULL,
    peer   TEXT NOT NULL DEFAULT '',
    kind   TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT ''
);
";

/// Checkpoints the WAL periodically, from its own connection and on its own thread.
///
/// `PASSIVE` is the key: it does what it can without blocking anyone and backs off if
/// someone is writing or reading. That way the WAL doesn't grow unbounded and no
/// click pays for the checkpoint.
pub fn spawn_checkpointer(path: &Path) {
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        let Ok(conn) = Connection::open(&path) else { return };
        loop {
            std::thread::sleep(std::time::Duration::from_secs(20));
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
        }
    });
}

/// Separate connection, only for reading what's shown on screen.
///
/// In WAL a reader doesn't wait for the writer: it sees the last checkpointed state
/// while the other one writes. With a single connection behind a mutex that's
/// lost, and opening a playlist ends up queued behind whatever sync is
/// doing — which is bursts of SQL between network trips.
///
/// `query_only` is so the separation doesn't rely on remembering: nothing can
/// write through here even if it tries.
pub fn open_read(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA query_only=ON; PRAGMA foreign_keys=ON;")?;
    Ok(conn)
}

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    // `synchronous` defaults to FULL: every commit does an fsync of the WAL. On a
    // phone's flash storage that's hundreds of milliseconds, and since there's a single
    // connection mutex for the whole app, that fsync isn't paid only by whoever is
    // writing — it queues up everything else. Marking a playlist is a commit.
    //
    // With WAL, NORMAL is still safe against a dirty shutdown or an app crash:
    // what's lost is at most the last commit if power is cut,
    // and the database doesn't get corrupted. It's what Android's own SQLiteDatabase
    // uses for the same reason.
    conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
    // Folds the previous session's WAL into the main file on every startup.
    // Without this, a dirty shutdown (force-kill in dev, a hang) leaves all the state
    // in the -wal; if a startup doesn't apply it, the library "appears empty".
    // TRUNCATE checkpoints and shrinks the -wal to zero.
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    // The automatic checkpoint is paid for by the commit that crosses the threshold: most
    // are cheap and every so often one checkpoints the whole WAL, with an fsync,
    // right on whichever click triggered it. That "it freezes every so often" is this.
    //
    // Here it's turned off and `spawn_checkpointer` does it instead from its own
    // connection, without stepping on anyone. The startup `wal_checkpoint(TRUNCATE)`
    // still covers the dirty shutdown, so the -wal never ends up unapplied.
    conn.execute_batch("PRAGMA wal_autocheckpoint=0;")?;
    init_schema(&conn)?;
    Ok(conn)
}

/// Creates the schema and applies the migrations on an already-open connection.
/// Used by `open` and other modules' tests, so nobody has to
/// maintain a copy of the DDL elsewhere.
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

/// Migrations on already-existing databases. `CREATE TABLE IF NOT EXISTS` doesn't
/// touch a table that's already there, so new columns get added here.
/// Idempotent: on a freshly created database SCHEMA already brings them and this does nothing.
fn migrate(conn: &Connection) -> Result<()> {
    // `position` INTEGER -> fractional `rank` (see rank.rs). The old column
    // is left where it is (it has a DEFAULT, doesn't get in the way of new INSERTs);
    // removing it would force rebuilding the whole table for no gain.
    for (table, group_by) in [("playlists", "parent_id"), ("playlist_tracks", "playlist_id")] {
        if has_column(conn, table, "rank")? {
            continue;
        }
        conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN rank TEXT NOT NULL DEFAULT ''"
        ))?;
        backfill_ranks(conn, table, group_by)?;
    }

    // Identity/sync columns. On a new database these already come from SCHEMA.
    let added: &[(&str, &str)] = &[
        ("tracks", "uid TEXT"),
        ("tracks", "content_hash TEXT"),
        ("tracks", "rel_path TEXT"),
        ("tracks", "size_bytes INTEGER"),
        ("tracks", "mtime_ms INTEGER"),
        ("tracks", "field_clocks TEXT"),
        ("tracks", "updated_at INTEGER NOT NULL DEFAULT 0"),
        ("tracks", "local_state TEXT NOT NULL DEFAULT 'present'"),
        // Playback levels: manual trim and what the analyzer measures.
        ("tracks", "gain_db REAL NOT NULL DEFAULT 0"),
        ("tracks", "loudness_lufs REAL"),
        ("tracks", "lead_silence_ms INTEGER"),
        ("tracks", "audio_end_ms INTEGER"),
        ("playlists", "uid TEXT"),
        ("playlists", "updated_at INTEGER NOT NULL DEFAULT 0"),
        ("playlist_tracks", "added_at INTEGER NOT NULL DEFAULT 0"),
        // Phase 5.7: scope becomes replicated data. On a 5.0-5.6 database
        // the table exists with two columns and no useful rows.
        ("sync_scope", "selected INTEGER NOT NULL DEFAULT 1"),
        ("sync_scope", "updated_at INTEGER NOT NULL DEFAULT 0"),
        // Phase 6.3: fixed address for devices that aren't self-discovered.
        ("devices", "address TEXT"),
        // Phase 6.9: how up to date we are on the other one's library, saved
        // so we don't have to compare everything on every app startup.
        ("devices", "watch_epoch INTEGER"),
        ("devices", "watch_rev INTEGER"),
    ];
    // Phase 6.4: per-device deletion policy was removed. It filtered by
    // who handed you the tombstone rather than who had deleted it, so with
    // three devices a deletion you rejected from one would still get in through another.
    // What protects things is the trash. The three tables were local — nothing
    // replicated references them — so they get dropped.
    conn.execute_batch(
        "DROP TABLE IF EXISTS sync_policy;
         DROP TABLE IF EXISTS pending_deletes;
         DROP TABLE IF EXISTS delete_ignores;",
    )?;
    for (table, decl) in added {
        // A table that doesn't exist yet needs no migration: SCHEMA
        // creates it. `migrate` also runs on its own in other modules' tests
        // against hand-built databases, where it can be missing.
        if !has_table(conn, table)? {
            continue;
        }
        let name = decl.split(' ').next().unwrap_or_default();
        if !has_column(conn, table, name)? {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {decl}"))?;
        }
    }
    // `device_scope` was called that when it only stored scope; now it
    // also stores direction, which isn't scope. It gets copied over and dropped.
    if has_table(conn, "device_scope")? {
        conn.execute_batch(
            "INSERT OR IGNORE INTO device_sync (device_uid, mode, direction, updated_at)
                SELECT device_uid, mode, 'both', updated_at FROM device_scope;
             DROP TABLE device_scope;",
        )?;
    }

    // The unique uid indexes go after the ALTER (SCHEMA runs before the
    // column exists on an old database, so that CREATE INDEX fails and gets skipped;
    // here it can already be created).
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_tracks_uid ON tracks(uid) WHERE uid IS NOT NULL;
         CREATE INDEX IF NOT EXISTS idx_tracks_hash ON tracks(content_hash);
         CREATE UNIQUE INDEX IF NOT EXISTS idx_playlists_uid ON playlists(uid) WHERE uid IS NOT NULL;
         -- The PK of playlist_tracks is (playlist_id, track_id): it serves for going
         -- from a playlist to its tracks, not the other way around. Everything about scope goes the
         -- other way — from a track to the playlists containing it — and without this each
         -- of those queries scans the whole table.
         CREATE INDEX IF NOT EXISTS idx_playlist_tracks_track ON playlist_tracks(track_id);
         CREATE INDEX IF NOT EXISTS idx_tracks_state ON tracks(local_state);",
    )?;
    assign_missing_uids(conn)?;
    // Memberships that already existed get dated NOW, not 0: if they stayed at
    // 0, any old tombstone for the same pair would win against them forever and the
    // next sync would remove them again. They're in the library now, so they count as of now.
    conn.execute(
        "UPDATE playlist_tracks SET added_at = ?1 WHERE added_at = 0",
        [now_ms()],
    )?;
    Ok(())
}

/// Every row without a `uid` gets one. Runs on every startup, not just during
/// migration: a row inserted through a path that forgot to generate it
/// (import, OS drop, watcher) still ends up syncable.
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

/// Milliseconds since epoch. The project doesn't use `chrono` (the XML date
/// is also built by hand in export_xml.rs), so this goes with `SystemTime`.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Translates the existing `position` order into ranks, group by group, without altering
/// the order the user was already seeing.
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
        "SELECT id, path, title, artist, album, genre, duration_ms, bpm, local_state, uid,
                gain_db, loudness_lufs
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
            gain_db: r.get(10)?,
            loudness_lufs: r.get(11)?,
        })
    })?;
    rows.collect()
}

pub fn track_path(conn: &Connection, id: i64) -> Result<String> {
    conn.query_row("SELECT path FROM tracks WHERE id = ?1", [id], |r| r.get(0))
}

// ---------------------------------------------------------------------------
// Playback levels
// ---------------------------------------------------------------------------

/// Everything about how one track should be played: how loud, and where its
/// audio actually starts and ends.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TrackPlayback {
    /// Manual trim set from the player.
    pub gain_db: f64,
    /// Measured loudness. `None` = not analyzed yet.
    pub loudness_lufs: Option<f64>,
    /// Absolute positions in ms where the audio starts and ends.
    /// `None` = not analyzed.
    pub lead_silence_ms: Option<i64>,
    pub audio_end_ms: Option<i64>,
}

pub fn track_playback(conn: &Connection, id: i64) -> Result<TrackPlayback> {
    conn.query_row(
        "SELECT gain_db, loudness_lufs, lead_silence_ms, audio_end_ms
         FROM tracks WHERE id = ?1",
        [id],
        |r| {
            Ok(TrackPlayback {
                gain_db: r.get(0)?,
                loudness_lufs: r.get(1)?,
                lead_silence_ms: r.get(2)?,
                audio_end_ms: r.get(3)?,
            })
        },
    )
}

pub fn set_track_gain(conn: &Connection, id: i64, gain_db: f64) -> Result<()> {
    conn.execute(
        "UPDATE tracks SET gain_db = ?2 WHERE id = ?1",
        rusqlite::params![id, gain_db],
    )?;
    Ok(())
}

/// Writes one analysis result. Loudness and the silence bounds are measured
/// in the same decode, so they are stored in the same statement — a row with
/// a loudness but no bounds would look analyzed while playing back untrimmed.
pub fn set_track_analysis(
    conn: &Connection,
    id: i64,
    lufs: f64,
    lead_ms: i64,
    audio_end_ms: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE tracks
            SET loudness_lufs = ?2, lead_silence_ms = ?3, audio_end_ms = ?4
          WHERE id = ?1",
        rusqlite::params![id, lufs, lead_ms, audio_end_ms],
    )?;
    Ok(())
}

/// What counts as still needing the analyzer.
///
/// Any measurement missing means the row gets measured again — they all come
/// out of one decode, so a row carrying some of them was written by a build
/// that measured less. Keying on loudness alone would leave every track
/// analyzed before the silence bounds existed permanently untrimmed, with
/// nothing in the UI to suggest why gapless did nothing.
///
/// `absent` rows are skipped throughout: measuring needs the bytes, and
/// selective sync means a device can hold thousands of rows with no audio
/// behind them.
const NEEDS_ANALYSIS: &str = "(loudness_lufs IS NULL
                              OR lead_silence_ms IS NULL
                              OR audio_end_ms IS NULL)
                             AND local_state = 'present'";

pub fn tracks_needing_analysis(conn: &Connection, limit: usize) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, path FROM tracks WHERE {NEEDS_ANALYSIS} ORDER BY id LIMIT ?1"
    ))?;
    let rows = stmt.query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

/// How many tracks are still waiting — what the progress line in Settings
/// counts down.
pub fn analysis_pending_count(conn: &Connection) -> Result<i64> {
    conn.query_row(
        &format!("SELECT COUNT(*) FROM tracks WHERE {NEEDS_ANALYSIS}"),
        [],
        |r| r.get(0),
    )
}

/// Throws away every measurement so the next sweep redoes the whole library.
/// What the "Rescan" button calls: the analyzer only ever looks at rows it
/// hasn't measured, so clearing the results is how you ask for them again.
pub fn clear_analysis(conn: &Connection) -> Result<usize> {
    let n = conn.execute(
        "UPDATE tracks
            SET loudness_lufs = NULL, lead_silence_ms = NULL, audio_end_ms = NULL",
        [],
    )?;
    Ok(n)
}

/// Deletes tracks from the library. Files living under the managed
/// folder `managed` get sent to the OS trash; ones outside it (legacy)
/// aren't touched. The CASCADE removes them from all playlists.
pub fn delete_tracks(conn: &mut Connection, managed: &std::path::Path, ids: &[i64]) -> Result<()> {
    // Gather paths and uids before deleting from the DB.
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
    // The tombstone is what stops the next sync from resurrecting it.
    for uid in uids {
        record_tombstone(conn, "track", &uid)?;
    }
    for p in paths {
        let path = std::path::Path::new(&p);
        if path.starts_with(managed) && path.exists() {
            // The `trash` crate doesn't support Android (no OS trash there).
            // Best-effort: don't break if it fails.
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
// Persisted config (app_settings)
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

/// Identity of THIS device, stable for life (generated once
/// and kept in `app_settings`). It's what signs tombstones and clocks, and what
/// breaks ties between equal LWW timestamps — that's why it can't be regenerated on every startup.
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

/// Visible name of this device. If what's stored is a placeholder from
/// an earlier version (the phone was literally named "Android") it gets
/// recomputed: the real name has only been discoverable since
/// `device_info` existed, and otherwise the phone would keep that name
/// forever.
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

/// Defaults to true: the whole point of Phase 2 is that the XML stays
/// synced on its own, without the user having to switch it on by hand.
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
// Playback preferences
//
// Local, not replicated: they describe this machine's audio output (which
// card, how much crossfade), not the library. The phone having its own
// crossfade is the correct behaviour, not drift to be reconciled.
// ---------------------------------------------------------------------------

/// The reference level normalization aims for.
///
/// -14 rather than the -18 of ReplayGain 2.0: a DJ library is mastered hot
/// (this one sits between -4 and -8 LUFS), and aiming at -18 pulled every
/// track down so far that the whole app just sounded quiet. -14 is the level
/// the streaming services settled on for the same reason, and it still leaves
/// 14 dB of headroom.
pub const TARGET_LUFS: f64 = -6.0;

/// Ceiling on how much anything may be turned **up**. Without it one badly
/// measured track (an ambient intro measured as the whole track) asks for
/// +40 dB and destroys the speakers. It is also the travel of the gain knob
/// in the player, in both directions.
pub const MAX_GAIN_DB: f64 = 12.0;

/// Floor on how much anything may be turned **down**. Deliberately further
/// from zero than the boost ceiling: attenuation cannot clip or blow
/// anything, and a modern club master sits 12-14 dB above the reference, so a
/// symmetric cap would leave exactly the loudest tracks — the ones
/// normalization is for — still louder than everything else.
pub const MAX_CUT_DB: f64 = 24.0;

#[derive(Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackPrefs {
    /// Seconds of overlap between tracks. 0 = off.
    pub crossfade_secs: f64,
    /// Feed the next track into the same output so there is no silence at the
    /// boundary. Moot while `crossfade_secs > 0` — an overlap has no gap to remove.
    pub gapless: bool,
    /// Continue to the next track when one ends. Off = playback stops there.
    pub autoplay: bool,
    /// Apply the measured loudness so every track lands near `TARGET_LUFS`.
    pub normalize: bool,
    /// Output device by name. `None` = follow whatever the system default is,
    /// including when the user changes it mid-track.
    pub output_device: Option<String>,
}

impl Default for PlaybackPrefs {
    fn default() -> Self {
        Self {
            crossfade_secs: 0.0,
            gapless: true,
            autoplay: true,
            normalize: false,
            output_device: None,
        }
    }
}

const SETTING_PLAYBACK: &str = "playback_prefs";

/// Reads the preferences, falling back to the defaults for anything missing.
/// Stored as one JSON blob rather than five keys: they are read and written
/// together, and a partial write would leave playback in a state no screen
/// ever showed.
pub fn get_playback_prefs(conn: &Connection) -> Result<PlaybackPrefs> {
    let raw = get_setting(conn, SETTING_PLAYBACK)?;
    Ok(raw
        .and_then(|s| serde_json::from_str::<StoredPrefs>(&s).ok())
        .map(Into::into)
        .unwrap_or_default())
}

pub fn set_playback_prefs(conn: &Connection, prefs: &PlaybackPrefs) -> Result<()> {
    let stored = StoredPrefs {
        crossfade_secs: Some(prefs.crossfade_secs),
        gapless: Some(prefs.gapless),
        autoplay: Some(prefs.autoplay),
        normalize: Some(prefs.normalize),
        output_device: prefs.output_device.clone(),
    };
    let json = serde_json::to_string(&stored).unwrap_or_default();
    set_setting(conn, SETTING_PLAYBACK, &json)
}

/// Every field optional so a blob written by an older version (which had
/// fewer keys) still loads, taking the default for what it doesn't carry.
///
/// Same key casing as the wire form above on purpose: one shape for this
/// data, whether it's being read out of the DB or off the API. Two casings
/// for the same five fields is a trap for whoever looks at the row next.
#[derive(serde::Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct StoredPrefs {
    crossfade_secs: Option<f64>,
    gapless: Option<bool>,
    autoplay: Option<bool>,
    normalize: Option<bool>,
    output_device: Option<String>,
}

impl From<StoredPrefs> for PlaybackPrefs {
    fn from(s: StoredPrefs) -> Self {
        let d = PlaybackPrefs::default();
        PlaybackPrefs {
            crossfade_secs: s.crossfade_secs.unwrap_or(d.crossfade_secs),
            gapless: s.gapless.unwrap_or(d.gapless),
            autoplay: s.autoplay.unwrap_or(d.autoplay),
            normalize: s.normalize.unwrap_or(d.normalize),
            output_device: s.output_device,
        }
    }
}

/// How loud to play a track, in dB relative to the file: the manual trim,
/// plus what normalization asks for when it's on and the track was measured.
///
/// The two add up rather than one replacing the other, which is how a DJ
/// mixer behaves: auto-gain sets the baseline, the trim knob is what you do
/// on top of it when the baseline is wrong for this room.
pub fn playback_gain_db(gain_db: f64, loudness_lufs: Option<f64>, normalize: bool) -> f64 {
    let auto = match (normalize, loudness_lufs) {
        (true, Some(lufs)) => TARGET_LUFS - lufs,
        // Not measured yet (or normalization off): the trim alone decides.
        _ => 0.0,
    };
    (gain_db + auto).clamp(-MAX_CUT_DB, MAX_GAIN_DB)
}

/// dB to the linear multiplier the audio sink wants.
pub fn db_to_linear(db: f64) -> f32 {
    10f64.powf(db / 20.0) as f32
}

// ---------------------------------------------------------------------------
// Playlists / folders (virtual hierarchy)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistNode {
    pub id: i64,
    /// Identity shared across devices. Needed by the scope editor
    /// (Phase 5.7): the `id` INTEGER is local and means nothing on the other side.
    pub uid: Option<String>,
    pub name: String,
    pub kind: String, // 'folder' | 'playlist'
    pub parent_id: Option<i64>,
    /// Index of the node among its siblings, derived from `rank`. The frontend
    /// sorts by this number; the rank itself never leaves Rust.
    pub position: i64,
    pub track_count: i64,
    /// Of those tracks, how many have a file present on this device. This is what
    /// shows when opening an unmarked playlist: as long as the file is there, the
    /// track shows dimmed, no matter why it's still there.
    pub present_count: i64,
    /// Of those tracks, how many are still taking up space BECAUSE OF THIS
    /// playlist: they have a file here and don't fall within this device's scope. This is what
    /// separates "unmarked but still taking up space" (still in the tree, dimmed) from
    /// "unmarked and already freed" (disappears).
    ///
    /// Deliberately different from `present_count`: a track that's also in a
    /// marked playlist is still SHOWN here, but doesn't count toward deciding whether this
    /// playlist keeps existing. Its file is being kept alive by the other one, so this
    /// one will never let it go, and counting it would keep it visible forever.
    ///
    /// Filled in by the command, not the query — it depends on the scope.
    pub stranded_count: i64,
    /// Whether this playlist falls within what this device syncs. Filled in by
    /// the command, not the query — it depends on the scope, not the row.
    pub in_scope: bool,
}

pub fn list_playlists(conn: &Connection) -> Result<Vec<PlaylistNode>> {
    let mut stmt = conn.prepare(
        // Both counts come from ONE grouped pass, not two subqueries
        // correlated per row: that used to mean 2N scans of playlist_tracks
        // per tree refresh, and the tree refreshes on every library
        // change.
        "SELECT p.id, p.uid, p.name, p.kind, p.parent_id,
                COALESCE(c.total, 0), COALESCE(c.present_rows, 0)
         FROM playlists p
         LEFT JOIN (
             SELECT pt.playlist_id AS pid,
                    COUNT(*) AS total,
                    COUNT(CASE WHEN t.local_state = 'present' THEN 1 END) AS present_rows
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
    // They already come grouped by parent and ordered by rank: just number from
    // zero within each group.
    let mut seen: std::collections::HashMap<Option<i64>, i64> = std::collections::HashMap::new();
    for n in nodes.iter_mut() {
        let slot = seen.entry(n.parent_id).or_insert(0);
        n.position = *slot;
        *slot += 1;
    }
    Ok(nodes)
}

/// Ranks of `parent`'s children, in order.
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
    // Children get deleted by ON DELETE CASCADE without going through here, so
    // their uids have to be gathered BEFORE that: a deletion without a tombstone gets
    // undone by the next sync (the other device still has it and resends
    // it).
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

/// Marks an entity as deleted. `INSERT OR REPLACE` so that a
/// re-deletion refreshes the timestamp instead of failing.
pub fn record_tombstone(conn: &Connection, entity: &str, uid: &str) -> Result<()> {
    let device = this_device_uid(conn)?;
    conn.execute(
        "INSERT OR REPLACE INTO tombstones (entity, uid, deleted_at, device_uid)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![entity, uid, now_ms(), device],
    )?;
    Ok(())
}

/// True if `maybe_ancestor` is an ancestor of (or equal to) `node`.
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

/// Moves a node to `new_parent` at index `index` among its siblings.
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
            return Err("the target is not a folder".into());
        }
        if is_ancestor(conn, id, p).map_err(|e| e.to_string())? {
            return Err("a folder cannot be moved inside itself".into());
        }
    }
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    {
        // Destination's siblings without the moved node: the new rank comes from
        // the two neighbors of the gap. No other row is touched.
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
// Tracks within a playlist
// ---------------------------------------------------------------------------

/// Ids of the playlists that contain the track.
pub fn track_playlists(conn: &Connection, track_id: i64) -> Result<Vec<i64>> {
    let mut stmt =
        conn.prepare("SELECT playlist_id FROM playlist_tracks WHERE track_id = ?1")?;
    let rows = stmt.query_map([track_id], |r| r.get(0))?;
    rows.collect()
}

pub fn playlist_tracks(conn: &Connection, playlist_id: i64) -> Result<Vec<Track>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.path, t.title, t.artist, t.album, t.genre, t.duration_ms, t.bpm,
                t.local_state, t.uid, t.gain_db, t.loudness_lufs
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
            gain_db: r.get(10)?,
            loudness_lufs: r.get(11)?,
        })
    })?;
    rows.collect()
}

/// Appends tracks at the end. Ignores ones already present. Returns how many it added.
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
        // Only advances if it was inserted: an ignored duplicate doesn't consume a rank.
        if n > 0 {
            last = Some(rank);
        }
        added += n;
    }
    touch_playlist(&tx, playlist_id)?;
    tx.commit()?;
    // Re-adding a track that had been removed has to lift its
    // tombstone; otherwise the pair stays dead forever and the next
    // sync removes it again.
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
    // No reindexing: the ranks of the ones that remain are still valid and
    // ordered relative to each other. Removing from the middle doesn't move anyone's rank.
    touch_playlist(&tx, playlist_id)?;
    tx.commit()?;
    // Membership merges by union (adding beats a concurrent removal),
    // so removing a track only propagates if there's explicit record of it.
    for p in pairs {
        record_tombstone(conn, "playlist_track", &p)?;
    }
    Ok(())
}

/// Moves a block of tracks (in their current order) to index `index`.
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
    // The requested index is over the full list; the moved
    // elements that were before the destination need to be subtracted.
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
    // Only the ranks of the moved block get rewritten; the ones that stay
    // keep theirs, which is what makes the merge not stomp on anything.
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

/// Marks the playlist as modified. It's the clock the merge uses to
/// decide whose order wins when both sides reordered: memberships
/// don't have their own timestamp, the playlist does.
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

    /// The trim is what the user set; normalization off means nothing else
    /// gets a say, even on a track that was measured.
    #[test]
    fn with_normalization_off_only_the_manual_trim_counts() {
        assert_eq!(playback_gain_db(0.0, Some(-30.0), false), 0.0);
        assert_eq!(playback_gain_db(3.5, Some(-30.0), false), 3.5);
        assert_eq!(playback_gain_db(-6.0, None, false), -6.0);
    }

    /// A track quieter than the target gets pushed up by exactly the
    /// difference, and one louder gets pulled down.
    #[test]
    fn normalization_moves_the_track_to_the_target_level() {
        assert_eq!(playback_gain_db(0.0, Some(TARGET_LUFS), true), 0.0);
        // -22 LUFS against a -14 target: 8 dB short.
        assert_eq!(playback_gain_db(0.0, Some(-22.0), true), 8.0);
        // -8 LUFS is 6 dB too loud.
        assert_eq!(playback_gain_db(0.0, Some(-8.0), true), -6.0);
    }

    /// The trim rides on top of normalization rather than replacing it —
    /// that's the point of having both, and it's how a mixer behaves.
    #[test]
    fn the_trim_adds_on_top_of_normalization() {
        assert_eq!(playback_gain_db(2.0, Some(-18.0), true), 6.0);
        assert_eq!(playback_gain_db(-2.0, Some(-18.0), true), 2.0);
    }

    /// An unmeasured track is left alone rather than guessed at: asking for a
    /// correction against a level nobody measured is how you get a boost that
    /// no one intended.
    #[test]
    fn an_unmeasured_track_is_not_corrected() {
        assert_eq!(playback_gain_db(0.0, None, true), 0.0);
        assert_eq!(playback_gain_db(4.0, None, true), 4.0);
    }

    /// One badly measured track (a long quiet intro read as the whole thing)
    /// must not be able to ask for a level that destroys the speakers.
    #[test]
    fn the_boost_is_capped() {
        assert_eq!(playback_gain_db(0.0, Some(-60.0), true), MAX_GAIN_DB);
        assert_eq!(playback_gain_db(100.0, None, false), MAX_GAIN_DB);
    }

    /// Turning down is capped much further out, because it cannot clip and a
    /// club master really does sit 12-14 dB above the reference. A symmetric
    /// cap would leave the loudest tracks — the ones normalization exists
    /// for — still standing out.
    #[test]
    fn a_loud_master_is_brought_all_the_way_down() {
        // A real track out of this library: -4.2 LUFS wants -9.8 dB against
        // the -14 reference.
        assert!((playback_gain_db(0.0, Some(-4.2), true) - (-9.8)).abs() < 1e-9);
        // The floor is still there for a nonsense measurement.
        assert_eq!(playback_gain_db(0.0, Some(40.0), true), -MAX_CUT_DB);
    }

    #[test]
    fn decibels_convert_to_the_multiplier_the_sink_wants() {
        assert!((db_to_linear(0.0) - 1.0).abs() < 1e-6);
        // +6 dB is roughly double the amplitude, -6 dB roughly half.
        assert!((db_to_linear(6.0) - 1.995).abs() < 0.01);
        assert!((db_to_linear(-6.0) - 0.501).abs() < 0.01);
    }

    /// Round trip through the DB: the trim sticks to the track and the
    /// measurements land next to it.
    #[test]
    fn gain_and_analysis_are_stored_per_track() {
        let conn = mem();
        let a = add_track(&conn, "/music/a.flac");
        let b = add_track(&conn, "/music/b.flac");
        assert_eq!(track_playback(&conn, a).unwrap(), TrackPlayback::default());

        set_track_gain(&conn, a, 4.5).unwrap();
        set_track_analysis(&conn, a, -21.0, 120, 340).unwrap();
        assert_eq!(
            track_playback(&conn, a).unwrap(),
            TrackPlayback {
                gain_db: 4.5,
                loudness_lufs: Some(-21.0),
                lead_silence_ms: Some(120),
                audio_end_ms: Some(340),
            }
        );
        // The other track is untouched.
        assert_eq!(track_playback(&conn, b).unwrap(), TrackPlayback::default());
    }

    /// The sweep only offers tracks whose bytes are actually here: measuring
    /// needs the file, and selective sync leaves rows without one.
    #[test]
    fn only_present_and_unmeasured_tracks_are_queued_for_analysis() {
        let conn = mem();
        let here = add_track(&conn, "/music/here.flac");
        let done = add_track(&conn, "/music/done.flac");
        let gone = add_track(&conn, "/music/gone.flac");
        set_track_analysis(&conn, done, -18.0, 0, 0).unwrap();
        conn.execute("UPDATE tracks SET local_state = 'absent' WHERE id = ?1", [gone])
            .unwrap();

        let pending = tracks_needing_analysis(&conn, 100).unwrap();
        assert_eq!(pending.len(), 1, "only the present, unmeasured one");
        assert_eq!(pending[0].0, here);
        assert_eq!(analysis_pending_count(&conn).unwrap(), 1);

        set_track_analysis(&conn, here, -20.0, 0, 0).unwrap();
        assert_eq!(analysis_pending_count(&conn).unwrap(), 0);
    }

    /// A row written by a build that measured less gets picked up again on
    /// its own. Without this, every track analyzed before the silence bounds
    /// existed would stay untrimmed forever and gapless would quietly do
    /// nothing.
    #[test]
    fn a_partly_measured_track_is_analyzed_again() {
        let conn = mem();
        let old = add_track(&conn, "/music/old.flac");
        // Exactly what the previous version left behind: loudness, no bounds.
        conn.execute("UPDATE tracks SET loudness_lufs = -8.0 WHERE id = ?1", [old])
            .unwrap();

        assert_eq!(analysis_pending_count(&conn).unwrap(), 1);
        assert_eq!(tracks_needing_analysis(&conn, 10).unwrap()[0].0, old);

        set_track_analysis(&conn, old, -8.0, 40, 900).unwrap();
        assert_eq!(analysis_pending_count(&conn).unwrap(), 0);
    }

    /// Rescan puts everything back in the queue — that's the whole mechanism
    /// behind the button, since the sweep only looks at unmeasured rows.
    #[test]
    fn rescan_puts_every_track_back_in_the_queue() {
        let conn = mem();
        let a = add_track(&conn, "/music/a.flac");
        let b = add_track(&conn, "/music/b.flac");
        set_track_analysis(&conn, a, -20.0, 100, 200).unwrap();
        set_track_analysis(&conn, b, TARGET_LUFS, 0, 0).unwrap();
        set_track_gain(&conn, a, 3.0).unwrap();
        assert_eq!(analysis_pending_count(&conn).unwrap(), 0);

        clear_analysis(&conn).unwrap();
        assert_eq!(analysis_pending_count(&conn).unwrap(), 2);
        // The manual trim is not a measurement and must survive a rescan.
        assert_eq!(track_playback(&conn, a).unwrap().gain_db, 3.0);
        assert_eq!(track_playback(&conn, a).unwrap().lead_silence_ms, None);
    }

    /// Preferences survive a round trip, and a blob written before a field
    /// existed still loads — taking the default for what it doesn't carry.
    #[test]
    fn playback_prefs_round_trip_and_tolerate_older_blobs() {
        let conn = mem();
        assert_eq!(get_playback_prefs(&conn).unwrap(), PlaybackPrefs::default());

        let mine = PlaybackPrefs {
            crossfade_secs: 6.0,
            gapless: false,
            autoplay: false,
            normalize: true,
            output_device: Some("Focusrite".into()),
        };
        set_playback_prefs(&conn, &mine).unwrap();
        assert_eq!(get_playback_prefs(&conn).unwrap(), mine);

        // A blob from a version that only knew about crossfade.
        set_setting(&conn, SETTING_PLAYBACK, r#"{"crossfadeSecs":3.0}"#).unwrap();
        let loaded = get_playback_prefs(&conn).unwrap();
        assert_eq!(loaded.crossfade_secs, 3.0);
        assert_eq!(loaded.gapless, PlaybackPrefs::default().gapless);
        assert_eq!(loaded.output_device, None);
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
        // a goes inside f
        move_playlist(&mut conn, a, Some(f), 0).unwrap();
        let nodes = list_playlists(&conn).unwrap();
        assert_eq!(nodes.iter().find(|n| n.id == a).unwrap().parent_id, Some(f));
        // b before f at the root
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
        assert!(move_playlist(&mut conn, f1, Some(f2), 0).is_err()); // cycle
        assert!(move_playlist(&mut conn, f1, Some(f1), 0).is_err()); // into itself
        assert!(move_playlist(&mut conn, f2, Some(p), 0).is_err()); // playlist is not a folder
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

        // Move the first one to the end (index = len).
        reorder_playlist_tracks(&mut conn, pl, &[ids[0]], 5).unwrap();
        assert_eq!(order_of(&conn, pl), vec![ids[1], ids[2], ids[3], ids[4], ids[0]]);

        // Move block {ids[3], ids[4]} (positions 2,3 now) to the start.
        reorder_playlist_tracks(&mut conn, pl, &[ids[3], ids[4]], 0).unwrap();
        assert_eq!(order_of(&conn, pl), vec![ids[3], ids[4], ids[1], ids[2], ids[0]]);

        // Move to the middle: ids[0] to index 2.
        reorder_playlist_tracks(&mut conn, pl, &[ids[0]], 2).unwrap();
        assert_eq!(order_of(&conn, pl), vec![ids[3], ids[4], ids[0], ids[1], ids[2]]);
    }

    #[test]
    fn remove_keeps_order_of_survivors() {
        let mut conn = mem();
        let pl = create_playlist(&conn, "p", "playlist", None).unwrap();
        let ids: Vec<i64> = (0..3).map(|i| add_track(&conn, &format!("/r{i}"))).collect();
        add_tracks_to_playlist(&mut conn, pl, &ids).unwrap();
        // Removes the middle one: the ranks of the other two aren't touched and the
        // relative order is preserved.
        let ranks_before = ranks_of(&conn, pl);
        remove_tracks_from_playlist(&mut conn, pl, &[ids[1]]).unwrap();
        assert_eq!(order_of(&conn, pl), vec![ids[0], ids[2]]);
        assert_eq!(ranks_of(&conn, pl), vec![ranks_before[0].clone(), ranks_before[2].clone()]);
    }

    /// Reordering should only rewrite the rank of the moved block. This is the
    /// property that makes two devices reordering offline merge
    /// without stomping on each other — if everything got renumbered, every reorder would touch every row.
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
            assert_eq!(before[id], after[id], "track {id} was not moved, its rank must not change");
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

    /// An old database (with `position` INTEGER and no `rank`) has to come out of
    /// the migration with exactly the same order the user was seeing.
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
        // Idempotent: running it again doesn't rewrite anything.
        let ranks = ranks_of(&conn, 1);
        migrate(&conn).unwrap();
        assert_eq!(ranks_of(&conn, 1), ranks);
    }

    /// Without a tombstone, the next sync sees that the other device still has
    /// the row and resends it: the deleted item reappears. That's why every deletion leaves a
    /// record, including the children the CASCADE takes down with it.
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

        // Adding it back has to lift the tombstone: otherwise the merge
        // union removes it again on the next sync.
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
        delete_tracks(&mut conn, std::path::Path::new("/nothing"), &[t1, t2]).unwrap();
        assert_eq!(tombstones_of(&conn, "track"), uids);
    }

    #[test]
    fn uids_are_assigned_to_legacy_rows_and_are_stable() {
        let conn = mem();
        conn.execute("INSERT INTO tracks (path) VALUES ('/old')", []).unwrap();
        conn.execute("INSERT INTO playlists (name, rank) VALUES ('old', 'V')", []).unwrap();
        assign_missing_uids(&conn).unwrap();

        let tuid: Option<String> = conn
            .query_row("SELECT uid FROM tracks WHERE path = '/old'", [], |r| r.get(0))
            .unwrap();
        assert!(tuid.is_some());
        // Second run: doesn't reassign (the uid has to be stable for
        // life, it's what playlists and tombstones reference).
        assign_missing_uids(&conn).unwrap();
        let again: Option<String> = conn
            .query_row("SELECT uid FROM tracks WHERE path = '/old'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tuid, again);
    }

    #[test]
    fn device_identity_is_generated_once() {
        let conn = mem();
        let uid = this_device_uid(&conn).unwrap();
        assert!(!uid.is_empty());
        assert_eq!(this_device_uid(&conn).unwrap(), uid);
        set_device_name(&conn, "Living room PC").unwrap();
        assert_eq!(device_name(&conn).unwrap(), "Living room PC");
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
        // The track is still in the library.
        assert_eq!(list_tracks(&conn).unwrap().len(), 1);
    }
}
