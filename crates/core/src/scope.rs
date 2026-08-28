//! Selective sync: which playlists each device wants (Phase 5.7).
//!
//! Scope **is replicated data**, not local config. It's edited from any
//! device — the PC decides what the phone downloads — and it travels like
//! everything else, with an `updated_at` per row and the newest wins. The
//! reason is that scope describes a wish ("I want these playlists on the
//! phone"), not a security rule. Deletion policy, which does protect, stays
//! local and is only edited on the device that protects.
//!
//! Two things scope deliberately does NOT do:
//!
//! - **It doesn't filter the data, it filters the view.** Playlists, order
//!   and metadata always replicate in full; the only selective thing is the
//!   audio files. Filtering the rows too would leave the device not knowing
//!   what exists, and there'd be nothing to come back to.
//!
//!   What does change is what's shown, and it depends on whether the file
//!   is still there: out of scope **with** a file shows dimmed and can't be
//!   used (it's on its way out); out of scope **without** a file — already
//!   freed — disappears from the main view. The scope editor, on the other
//!   hand, always shows everything: it's the only place from which what was
//!   hidden can be re-marked, so hiding it there would be a dead end.
//! - **It doesn't delete.** Unchecking just cuts the sync and nothing else:
//!   files that are already there stay where they are. Freeing up space is
//!   a separate, explicit action (`evictable` / `evict`), because unchecking
//!   is often a slip of the finger and nobody wants a click to silently
//!   sweep away 2 GB.

use crate::manifest::{Membership, PlaylistEntry, ScopeEntry, DeviceSync};
use rusqlite::{Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Everything in the library.
    All,
    /// Only what hangs off the marked playlists/folders.
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

/// What a device does: sends, receives, both, or neither.
///
/// It's a property **of the device**, not of the link: the PC sends and
/// receives, the phone only receives, and that holds regardless of who it's
/// paired with. Between two devices, A → B happens only if A sends **and** B
/// receives. Since it's replicated data, both sides read the same two rows
/// and reach the same conclusion without negotiating anything.
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

/// What a device wants: direction, mode, and the manually marked uids.
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

/// What can move between two devices: `takes` = `local` pulls from `remote`,
/// `gives` = `local` sends to `remote`.
pub fn link(local: &Direction, remote: &Direction) -> (bool, bool) {
    (local.receives && remote.sends, local.sends && remote.receives)
}

/// Same, but resolving both directions from the two manifests' replicated
/// rows (each device's newest wins).
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
// Resolution (pure)
// ---------------------------------------------------------------------------

/// Marking a folder marks everything that hangs off it. Otherwise marking
/// "Sets" wouldn't bring in any of the playlists inside it and the tree
/// would be purely decorative.
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
            continue; // already visited — this also breaks a cycle if there were one
        }
        if let Some(kids) = children.get(uid) {
            stack.extend(kids.iter().copied());
        }
    }
    out
}

/// Which tracks fall within scope. `None` means "all": that's different from
/// an empty set, which means "none".
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

/// Rebuilds a device's scope from already-merged replicated rows (the ones
/// that travel in the manifest).
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

/// Merges the scope rows from both sides, keeping the newest of each. Used
/// for planning: during the window where a scope change hasn't traveled yet,
/// the two manifests differ and the decision has to go with the most recent
/// one, not the local one.
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
// Reading and writing to the DB
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

/// What that device does. Edited from either side, same as scope: it's a
/// preference, not a defense.
pub fn set_direction(conn: &Connection, device_uid: &str, direction: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO device_sync (device_uid, direction, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(device_uid) DO UPDATE SET direction = excluded.direction,
                                               updated_at = excluded.updated_at",
        rusqlite::params![device_uid, direction, crate::db::now_ms()],
    )?;
    Ok(())
}

/// Marks or unmarks a playlist. Unmarking leaves the row with `selected = 0`:
/// deleting it would let the merge's union bring it back from the other side.
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

/// A playlist created by the user on this device starts out marked.
///
/// Without this, in selective mode, creating it would make it vanish from
/// the tree instantly: it's born with no scope row — i.e. unmarked — and no
/// files, which is exactly the combination that gets hidden. This only
/// applies to playlists created here: the ones arriving via sync stay
/// unmarked, which is what makes sync selective.
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

/// Applies a row that arrived from the other side. Returns `true` if
/// something changed.
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

/// LWW over the whole row: mode and direction travel together. A direction
/// change made on one device can override a mode change made on the other at
/// the same time — same rule as the rest of this phase, and the alternative
/// (a clock per field) isn't worth it for two settings that change once in a
/// while.
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
// Known replicas of each blob
// ---------------------------------------------------------------------------

/// Notes that `device_uid` has those files. It's the only thing that makes
/// freeing up space safe: without a confirmed copy somewhere else, evicting a
/// file could be destroying the last copy.
/// All of it goes in ONE transaction with the statement prepared just once:
/// there are as many rows as the other device has tracks, and in autocommit
/// every INSERT is an fsync. With a thousand-track library that's a thousand
/// fsyncs per sync, with the DB lock held — on the phone it feels like the
/// app froze.
pub fn note_replicas(conn: &Connection, device_uid: &str, hashes: &[String]) -> rusqlite::Result<()> {
    if hashes.is_empty() {
        return Ok(());
    }
    let now = crate::db::now_ms();
    conn.execute_batch("BEGIN")?;
    let result = (|| -> rusqlite::Result<()> {
        // The `WHERE` isn't cosmetic: without it, every sync would rewrite
        // one row per track of the other device even if nothing changed,
        // dirtying pages and making the WAL work for nothing. Refreshing the
        // timestamp once an hour is enough — this only answers "does anyone
        // else have it?".
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

/// Which tracks (by uid) are in scope for a device, read from the DB.
/// `None` = all.
///
/// Resolved in SQL — with a recursive CTE to walk down the tree — instead of
/// loading the membership table into memory: this is called by `list_tracks`
/// on every library refresh, and pulling in thousands of rows just to
/// discard them is noticeable on the phone. The rule is the same as
/// `tracks_in_scope`'s (there's a test that requires them to agree).
pub fn scope_tracks(conn: &Connection, device_uid: &str) -> rusqlite::Result<Option<HashSet<String>>> {
    let scope = get(conn, device_uid)?;
    if scope.mode == Mode::All {
        return Ok(None);
    }
    let mut stmt = conn.prepare_cached(
        "WITH RECURSIVE marked(uid) AS (
             SELECT playlist_uid FROM sync_scope
              WHERE device_uid = ?1 AND selected = 1
             UNION
             SELECT child.uid FROM playlists child
               JOIN playlists parent ON parent.id = child.parent_id
               JOIN marked ON marked.uid = parent.uid
             WHERE child.uid IS NOT NULL
         )
         SELECT DISTINCT t.uid
           FROM playlist_tracks pt
           JOIN playlists p ON p.id = pt.playlist_id
           JOIN tracks t ON t.id = pt.track_id
          WHERE t.uid IS NOT NULL AND p.uid IN (SELECT uid FROM marked)",
    )?;
    let rows = stmt.query_map([device_uid], |r| r.get::<_, String>(0))?;
    Ok(Some(rows.collect::<rusqlite::Result<HashSet<_>>>()?))
}

/// Same as `scope_tracks`, but looking only at the tracks of ONE playlist.
///
/// Opening a playlist used to call `scope_tracks`, which builds the in-scope
/// set for the ENTIRE library — walking every membership, with a DISTINCT
/// over uids — just to then mark twenty rows. With the DB lock held. Here it
/// starts from that playlist's rows, which are few, and reaches out through
/// `playlist_tracks(track_id)` to see if any of the track's other playlists
/// is marked.
pub fn scope_tracks_of_playlist(
    conn: &Connection,
    device_uid: &str,
    playlist_id: i64,
) -> rusqlite::Result<Option<HashSet<String>>> {
    if get(conn, device_uid)?.mode == Mode::All {
        return Ok(None);
    }
    let mut stmt = conn.prepare_cached(
        "WITH RECURSIVE marked(uid) AS (
             SELECT playlist_uid FROM sync_scope
              WHERE device_uid = ?1 AND selected = 1
             UNION
             SELECT child.uid FROM playlists child
               JOIN playlists parent ON parent.id = child.parent_id
               JOIN marked ON marked.uid = parent.uid
             WHERE child.uid IS NOT NULL
         )
         SELECT DISTINCT t.uid
           FROM playlist_tracks here
           JOIN playlist_tracks others ON others.track_id = here.track_id
           JOIN playlists p ON p.id = others.playlist_id
           JOIN tracks t ON t.id = here.track_id
          WHERE here.playlist_id = ?2
            AND t.uid IS NOT NULL
            AND p.uid IN (SELECT uid FROM marked)",
    )?;
    let rows = stmt.query_map(rusqlite::params![device_uid, playlist_id], |r| {
        r.get::<_, String>(0)
    })?;
    Ok(Some(rows.collect::<rusqlite::Result<HashSet<_>>>()?))
}

/// Which playlists (by uid) are marked for a device, already expanded down
/// the tree. `None` = all.
///
/// Watch out for folders: a folder that isn't marked but contains a marked
/// playlist does NOT come out of here, and yet it still has to be visible —
/// otherwise the marked one inside is orphaned with no way to reach it. The
/// frontend's tree handles that part, showing any node with a visible
/// descendant.
pub fn scope_playlists(
    conn: &Connection,
    device_uid: &str,
) -> rusqlite::Result<Option<HashSet<String>>> {
    if get(conn, device_uid)?.mode == Mode::All {
        return Ok(None);
    }
    let mut stmt = conn.prepare_cached(
        "WITH RECURSIVE marked(uid) AS (
             SELECT playlist_uid FROM sync_scope
              WHERE device_uid = ?1 AND selected = 1
             UNION
             SELECT child.uid FROM playlists child
               JOIN playlists parent ON parent.id = child.parent_id
               JOIN marked ON marked.uid = parent.uid
             WHERE child.uid IS NOT NULL
         )
         SELECT uid FROM marked",
    )?;
    let rows = stmt.query_map([device_uid], |r| r.get::<_, String>(0))?;
    Ok(Some(rows.collect::<rusqlite::Result<HashSet<_>>>()?))
}

/// Per playlist, how many of its tracks still take up space **because of
/// it**: they have a file here and aren't in this device's scope. Empty if
/// scope is "all".
///
/// Just counting the ones with a file isn't enough. A track that's in a
/// marked playlist and in an unmarked one has a file here because of the
/// marked one: the unmarked one isn't holding it up and will never let it
/// go, so counting it would leave it visible forever. What counts are the
/// ones that will actually leave once space is freed up.
///
/// This is only for deciding whether the playlist stays in the tree. What
/// gets shown INSIDE it is a different matter: everything with a file shows
/// there, borrowed or not, because hiding something that's still taking up
/// space would be lying about the space.
pub fn stranded_counts(
    conn: &Connection,
    device_uid: &str,
) -> rusqlite::Result<HashMap<i64, i64>> {
    if get(conn, device_uid)?.mode == Mode::All {
        return Ok(HashMap::new());
    }
    let mut stmt = conn.prepare_cached(
        "WITH RECURSIVE marked(uid) AS (
             SELECT playlist_uid FROM sync_scope
              WHERE device_uid = ?1 AND selected = 1
             UNION
             SELECT child.uid FROM playlists child
               JOIN playlists parent ON parent.id = child.parent_id
               JOIN marked ON marked.uid = parent.uid
             WHERE child.uid IS NOT NULL
         ),
         held(track_id) AS (
             SELECT DISTINCT pt.track_id
               FROM playlist_tracks pt
               JOIN playlists p ON p.id = pt.playlist_id
              WHERE p.uid IN (SELECT uid FROM marked)
         )
         SELECT pt.playlist_id, COUNT(*)
           FROM playlist_tracks pt
           JOIN tracks t ON t.id = pt.track_id
          WHERE t.local_state = 'present'
            AND pt.track_id NOT IN (SELECT track_id FROM held)
          GROUP BY pt.playlist_id",
    )?;
    let rows = stmt.query_map([device_uid], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// Freeing up space
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Evictable {
    pub id: i64,
    pub path: String,
    pub size: i64,
}

/// Files this device could let go of: they're here, they fell outside
/// scope, and **it's confirmed they live on another linked device**.
///
/// That last requirement is what keeps "Free up space" from losing music. A
/// track outside scope that isn't anywhere else doesn't get offered: the
/// only copy stays where it is.
pub fn evictable(
    conn: &Connection,
    music_dir: &std::path::Path,
) -> rusqlite::Result<Vec<Evictable>> {
    let me = crate::db::this_device_uid(conn)?;
    if get(conn, &me)?.mode == Mode::All {
        return Ok(Vec::new()); // scope = all: nothing is extra
    }

    // All the discarding in ONE query, and the anti-join by `track_id`
    // (integer) instead of by `uid` (text).
    //
    // Before, this brought EVERY present track in the library into Rust just
    // to discard them in a `for`, and it also built the entire in-scope set
    // with a DISTINCT over uids. With the DB lock held, which is global:
    // while it ran, anything else touching the DB — opening a playlist,
    // starting a track — got queued up. That's what made opening the sync
    // panel freeze the whole app.
    let mut stmt = conn.prepare_cached(
        "WITH RECURSIVE marked(uid) AS (
             SELECT playlist_uid FROM sync_scope
              WHERE device_uid = ?1 AND selected = 1
             UNION
             SELECT child.uid FROM playlists child
               JOIN playlists parent ON parent.id = child.parent_id
               JOIN marked ON marked.uid = parent.uid
             WHERE child.uid IS NOT NULL
         ),
         in_scope(track_id) AS (
             SELECT DISTINCT pt.track_id
               FROM playlist_tracks pt
               JOIN playlists p ON p.id = pt.playlist_id
              WHERE p.uid IN (SELECT uid FROM marked)
         )
         SELECT t.id, t.path, COALESCE(t.size_bytes, 0)
           FROM tracks t
          WHERE t.local_state = 'present' AND t.uid IS NOT NULL
            AND t.content_hash IS NOT NULL
            AND t.id NOT IN (SELECT track_id FROM in_scope)
            AND EXISTS (SELECT 1 FROM blob_replicas r
                         WHERE r.hash = t.content_hash AND r.device_uid <> ?1)",
    )?;
    let rows = stmt.query_map([&me], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, path, size) = row?;
        // A legacy file outside the managed folder isn't ours to move.
        // Over the few rows that survived, not over all of them.
        if !std::path::Path::new(&path).starts_with(music_dir) {
            continue;
        }
        out.push(Evictable { id, path, size });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Rescue from the trash
// ---------------------------------------------------------------------------
//
// Re-marking a freed playlist can't re-download over the network files that
// are still sitting in `trash`, one `rename` away. But verification is
// by hash, and hashing is expensive, so the work is split into three parts
// — finding candidates, hashing, applying — because **the hash CANNOT be
// computed while the DB lock is held**. Holding it while gigabytes get
// hashed freezes the whole UI: every `list_tracks` call from the frontend
// waits on the mutex. It's the exact same reason the hash backfill releases
// the lock between files.

/// A row with no file that would need to be recovered.
#[derive(Debug, Clone)]
pub struct Restorable {
    pub id: i64,
    pub hash: String,
    pub rel_path: String,
    pub size: i64,
}

/// What's missing and might be in the trash. Only queries the DB — cheap.
pub fn restorable(conn: &Connection) -> rusqlite::Result<Vec<Restorable>> {
    let me = crate::db::this_device_uid(conn)?;
    let selective = get(conn, &me)?.mode == Mode::Selected;
    // The scope filter goes in SQL, same as in `evictable`: this runs on
    // every sync and every scope change, with the global lock held.
    let sql = if selective {
        "WITH RECURSIVE marked(uid) AS (
             SELECT playlist_uid FROM sync_scope
              WHERE device_uid = ?1 AND selected = 1
             UNION
             SELECT child.uid FROM playlists child
               JOIN playlists parent ON parent.id = child.parent_id
               JOIN marked ON marked.uid = parent.uid
             WHERE child.uid IS NOT NULL
         ),
         in_scope(track_id) AS (
             SELECT DISTINCT pt.track_id
               FROM playlist_tracks pt
               JOIN playlists p ON p.id = pt.playlist_id
              WHERE p.uid IN (SELECT uid FROM marked)
         )
         SELECT id, content_hash, rel_path, COALESCE(size_bytes, 0)
           FROM tracks
          WHERE local_state <> 'present' AND uid IS NOT NULL
            AND content_hash IS NOT NULL AND COALESCE(rel_path, '') <> ''
            AND id IN (SELECT track_id FROM in_scope)"
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

/// Looks for those files in the trash. **Hashes: run without the lock.**
///
/// The trash index is built ONCE, grouped by size. Before, the directory was
/// re-read per candidate, so N missing files against M files in the trash
/// gave N×M hashes — with a full trash, minutes of CPU on every sync.
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
    // size -> trash files with that size, with their timestamp.
    let t_scan = std::time::Instant::now();
    let mut by_size: HashMap<u64, Vec<(std::path::PathBuf, i64)>> = HashMap::new();
    let mut n_files = 0;
    for e in entries.flatten() {
        let Ok(md) = e.metadata() else { continue };
        if md.is_file() {
            n_files += 1;
            by_size.entry(md.len()).or_default().push((e.path(), mtime_of(&md)));
        }
    }
    // TEMPORARY — one `stat` per file, and on Android the music folder lives
    // on emulated storage (FUSE), where that isn't free.
    crate::perf_line(&format!(
        "    find_in_trash: scan {} ms, {n_files} file(s)",
        t_scan.elapsed().as_millis()
    ));
    if by_size.is_empty() {
        return Vec::new();
    }

    let t_hash = std::time::Instant::now();
    let mut hashed = 0;
    let mut out = Vec::new();
    for c in candidates {
        let Some(same_size) = by_size.get(&(c.size as u64)) else {
            continue; // no candidate of the same size, nothing to hash
        };
        // Name first: the trash keeps it unless there was a collision.
        let preferred = trash.join(&c.rel_path);
        let order = same_size
            .iter()
            .filter(|(p, _)| *p == preferred)
            .chain(same_size.iter().filter(|(p, _)| *p != preferred));
        for (p, mtime) in order {
            hashed += 1;
            let Some(h) = trash_hash(p, c.size as u64, *mtime) else { continue };
            if h == c.hash {
                out.push((c.clone(), p.clone()));
                break;
            }
        }
    }
    crate::perf_line(&format!(
        "    find_in_trash: hashes {} ms, {hashed} cache lookup(s)",
        t_hash.elapsed().as_millis()
    ));
    out
}

fn mtime_of(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Hashes of trash files already computed, between calls.
///
/// This is the most expensive part of the whole phase, and it runs on EVERY
/// scope change: marking a playlist has to be able to recover what's already
/// there from the trash. Without a cache surviving the call, every click
/// would rehash the entire trash — which after freeing up space a few times
/// is hundreds of megabytes — and in an unoptimized build that's seconds of
/// CPU per click.
///
/// Worse: candidates that are NOT in the trash (a track in scope that was
/// never downloaded) never resolve, so the same work would repeat on every
/// click forever.
///
/// The key carries size and date: if the file changed, it gets rehashed.
static TRASH_HASHES: std::sync::OnceLock<
    std::sync::Mutex<HashMap<std::path::PathBuf, (u64, i64, String)>>,
> = std::sync::OnceLock::new();

fn trash_hash(path: &std::path::Path, len: u64, mtime: i64) -> Option<String> {
    let cache = TRASH_HASHES.get_or_init(Default::default);
    if let Ok(map) = cache.lock() {
        if let Some((l, m, h)) = map.get(path) {
            if *l == len && *m == mtime {
                return Some(h.clone());
            }
        }
    }
    let h = crate::hashing::hash_file(path).ok()?;
    if let Ok(mut map) = cache.lock() {
        map.insert(path.to_path_buf(), (len, mtime, h.clone()));
    }
    Some(h)
}

/// Moves what was found back into the library and updates the rows. Cheap:
/// renames and updates, no hashing.
///
/// `expect` marks the destination before writing it: the watcher observes
/// the managed folder and, without that, would auto-import the recovered
/// file as if it were new — new row, new uid, shared identity broken.
pub fn finish_restore(
    conn: &Connection,
    music_dir: &std::path::Path,
    found: &[(Restorable, std::path::PathBuf)],
    expect: &dyn Fn(&std::path::Path),
) -> rusqlite::Result<usize> {
    let mut restored = 0;
    for (c, src) in found {
        // If the managed folder already has a file with that name and size,
        // `managed_dest_for` returns that same one: it reappeared through
        // another path (a manual copy, a sync that arrived first) and all
        // that's left is to repoint the row. Otherwise, the one in the trash
        // gets moved.
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
        log::info!("[scope] {restored} file(s) restored from trash");
    }
    Ok(restored)
}

/// Runs the eviction: the files go to the library's trash (30 days) and the
/// rows are set to `absent`. The row is NOT deleted or tombstoned: the track
/// is still visible, grayed out, and re-marking its playlist downloads it
/// again.
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
                log::warn!("[scope] could not evict {}: {e}", path.display());
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
        log::info!("[scope] {n} file(s) evicted, {bytes} bytes freed");
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

    /// Marking a folder has to bring in what hangs off it, or the tree would
    /// be purely decorative.
    #[test]
    fn selecting_a_folder_selects_its_subtree() {
        let tree = vec![
            pl("folder", None),
            pl("inside", Some("folder")),
            pl("deeper-inside", Some("inside")),
            pl("outside", None),
        ];
        let got = expand(&tree, &set(&["folder"]));
        assert!(got.contains("inside") && got.contains("deeper-inside"));
        assert!(!got.contains("outside"));
    }

    /// Direction belongs to the device, not the link: what the PC does holds
    /// no matter who it's paired with, and between two devices something
    /// moves only if one sends and the other receives. Both sides read the
    /// same two rows and reach the same conclusion without negotiating over
    /// the network.
    #[test]
    fn a_device_that_only_receives_never_sends_to_anyone() {
        let pc = Direction::from_setting("both");
        let phone = Direction::from_setting("receive");

        // Seen from the PC: it sends to the phone, gets nothing back.
        assert_eq!(link(&pc, &phone), (false, true));
        // Seen from the phone: same conclusion, reversed.
        assert_eq!(link(&phone, &pc), (true, false));
        // Two that only receive move nothing.
        assert_eq!(link(&phone, &phone), (false, false));
        // And one that's paused is idle with everyone.
        assert_eq!(link(&pc, &Direction::from_setting("off")), (false, false));
    }

    #[test]
    fn mode_all_means_every_track_no_matter_the_selection() {
        let scope = Scope { mode: Mode::All, direction: Direction::default(), selected: set(&["x"]) };
        assert!(tracks_in_scope(&[], &[], &scope).is_none());
    }

    #[test]
    fn only_tracks_under_selected_playlists_are_in_scope() {
        let tree = vec![pl("yes", None), pl("no", None)];
        let members = vec![member("yes", "t1"), member("no", "t2")];
        let scope = Scope { mode: Mode::Selected, direction: Direction::default(), selected: set(&["yes"]) };
        let got = tracks_in_scope(&tree, &members, &scope).unwrap();
        assert!(got.contains("t1"));
        assert!(!got.contains("t2"));
    }

    /// Unmarking on the other side has to stick: the newest wins, not the
    /// local one.
    #[test]
    fn the_newest_scope_row_wins() {
        let mine = vec![ScopeEntry {
            device_uid: "phone".into(),
            playlist_uid: "sets".into(),
            selected: true,
            updated_at: 100,
        }];
        let theirs = vec![ScopeEntry {
            device_uid: "phone".into(),
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

    /// Unmarking leaves the row with `selected = 0`. If it deleted it, the
    /// merge's union would bring it back and unmarking would never stick.
    #[test]
    fn unselecting_keeps_a_row_that_says_no() {
        let conn = mem();
        set_playlist(&conn, "phone", "sets", true).unwrap();
        set_playlist(&conn, "phone", "sets", false).unwrap();
        let rows = entries(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].selected);
        assert!(!get(&conn, "phone").unwrap().selected.contains("sets"));
    }

    #[test]
    fn an_older_incoming_row_does_not_win() {
        let conn = mem();
        set_playlist(&conn, "phone", "sets", true).unwrap();
        let now = entries(&conn).unwrap()[0].updated_at;
        let applied = apply_entry(
            &conn,
            &ScopeEntry {
                device_uid: "phone".into(),
                playlist_uid: "sets".into(),
                selected: false,
                updated_at: now - 1000,
            },
        )
        .unwrap();
        assert!(!applied);
        assert!(entries(&conn).unwrap()[0].selected);
    }

    /// Full round trip of selective sync: free up space and mark it again.
    /// The file has to come out of the trash, not off the network — it's a
    /// `rename` away, and re-downloading it would be throwing the connection
    /// (and the phone's battery) out the window.
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
        let file = music.join("track.flac");
        std::fs::write(&file, b"real audio").unwrap();
        let hash = crate::hashing::hash_file(&file).unwrap();

        conn.execute(
            "INSERT INTO tracks (path, uid, content_hash, rel_path, size_bytes, local_state)
             VALUES (?1, 'tr', ?2, 'track.flac', 10, 'present')",
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
        note_replicas(&conn, "other", &[hash.clone()]).unwrap();

        // Selective scope with nothing marked: the file is extra and gets freed.
        set_mode(&conn, &me, Mode::Selected).unwrap();
        let items = evictable(&conn, &music).unwrap();
        assert_eq!(items.len(), 1);
        evict(&conn, &music, &items).unwrap();
        assert!(!file.exists(), "left the library");

        // The playlist gets marked again: it comes back from the trash.
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
        assert_eq!(std::fs::read(&path).unwrap(), b"real audio");
        std::fs::remove_dir_all(&music).ok();
    }

    /// There are two implementations of the same rule: the pure one
    /// (`tracks_in_scope`, the one that decides what gets transferred) and
    /// the SQL one (`scope_tracks`, the one that paints the library). If
    /// they disagreed, the table would show one thing and sync would do
    /// another.
    #[test]
    fn the_sql_and_the_pure_rule_agree() {
        let conn = mem();
        let me = db::this_device_uid(&conn).unwrap();
        // Folder > sub > playlist, with one track inside and one outside.
        let folder = db::create_playlist(&conn, "Folder", "folder", None).unwrap();
        let sub = db::create_playlist(&conn, "Sub", "playlist", Some(folder)).unwrap();
        let outside = db::create_playlist(&conn, "Outside", "playlist", None).unwrap();
        for (i, pl) in [(1, sub), (2, outside)] {
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
        let folder_uid: String = conn
            .query_row("SELECT uid FROM playlists WHERE id = ?1", [folder], |r| r.get(0))
            .unwrap();

        set_mode(&conn, &me, Mode::Selected).unwrap();
        set_playlist(&conn, &me, &folder_uid, true).unwrap();

        // SQL rule: marking the folder brings in the track from the playlist inside.
        let via_sql = scope_tracks(&conn, &me).unwrap().unwrap();
        assert!(via_sql.contains("t1"));
        assert!(!via_sql.contains("t2"));

        // Pure rule, over the same data: has to give the same result.
        let m = crate::manifest::build(&conn).unwrap();
        let via_pure =
            tracks_in_scope(&m.playlists, &m.memberships, &get(&conn, &me).unwrap()).unwrap();
        assert_eq!(via_sql, via_pure);
    }

    /// What decides what's shown in the main view's tree. Includes the case
    /// of a half-marked folder, which is the one that can leave a playlist
    /// visible but unreachable if the frontend doesn't do its part.
    #[test]
    fn selected_playlists_expand_down_but_not_up() {
        let conn = mem();
        let me = db::this_device_uid(&conn).unwrap();
        let folder = db::create_playlist(&conn, "Folder", "folder", None).unwrap();
        let inside = db::create_playlist(&conn, "Inside", "playlist", Some(folder)).unwrap();
        let outside = db::create_playlist(&conn, "Outside", "playlist", None).unwrap();
        let uid = |id: i64| -> String {
            conn.query_row("SELECT uid FROM playlists WHERE id = ?1", [id], |r| r.get(0))
                .unwrap()
        };

        // With no selective mode, nothing gets filtered.
        assert!(scope_playlists(&conn, &me).unwrap().is_none());

        set_mode(&conn, &me, Mode::Selected).unwrap();
        set_playlist(&conn, &me, &uid(inside), true).unwrap();
        let got = scope_playlists(&conn, &me).unwrap().unwrap();

        assert!(got.contains(&uid(inside)));
        assert!(!got.contains(&uid(outside)));
        // The folder containing it does NOT come in: marking a child doesn't
        // mark the parent. Still showing it in the tree is the frontend's
        // job, which displays any node with a visible descendant.
        assert!(!got.contains(&uid(folder)));

        // And the other way around it does flow down: marking the folder
        // brings in what hangs off it.
        set_playlist(&conn, &me, &uid(folder), true).unwrap();
        let got = scope_playlists(&conn, &me).unwrap().unwrap();
        assert!(got.contains(&uid(folder)) && got.contains(&uid(inside)));
        assert!(!got.contains(&uid(outside)));
    }

    /// The tree needs to tell apart "unmarked but still taking up space" from
    /// "unmarked and already freed": the first shows dimmed, the second
    /// disappears. And a track that's in both doesn't count for the
    /// unmarked one: its file is held up by the marked one, so the unmarked
    /// one will never let it go and would stay visible forever showing
    /// something borrowed.
    #[test]
    fn a_shared_track_does_not_keep_the_unselected_playlist_alive() {
        let conn = mem();
        let me = db::this_device_uid(&conn).unwrap();
        let marked = db::create_playlist(&conn, "Marked", "playlist", None).unwrap();
        let no = db::create_playlist(&conn, "Unmarked", "playlist", None).unwrap();
        let uid_of = |id: i64| -> String {
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
        // `shared` is in both. `own` is only in the unmarked one.
        add(marked, "shared", "present");
        add(no, "shared", "present");
        add(no, "own", "present");

        set_mode(&conn, &me, Mode::Selected).unwrap();
        set_playlist(&conn, &me, &uid_of(marked), true).unwrap();

        let c = stranded_counts(&conn, &me).unwrap();
        assert_eq!(c.get(&marked), None, "what's in scope doesn't count");
        assert_eq!(
            c.get(&no).copied().unwrap_or(0),
            1,
            "only `own`: `shared` is held up by the marked one"
        );

        // But both still have a file, so opening the unmarked one shows both,
        // dimmed: while they take up space, hiding them from a list where
        // they appear would be lying about the space.
        let node = db::list_playlists(&conn)
            .unwrap()
            .into_iter()
            .find(|n| n.id == no)
            .unwrap();
        assert_eq!(node.track_count, 2);
        assert_eq!(node.present_count, 2);

        // And `shared` is in scope: that's what keeps it from being dimmed or
        // hidden in the marked playlist, where it actually belongs.
        let in_scope = scope_tracks(&conn, &me).unwrap().unwrap();
        assert!(in_scope.contains("shared"));
        assert!(!in_scope.contains("own"));

        // Space gets freed: `own` leaves and the unmarked playlist stops
        // holding anything up, even though `shared` is still here because of
        // the other one.
        conn.execute(
            "UPDATE tracks SET local_state = 'absent' WHERE uid = 'own'",
            [],
        )
        .unwrap();
        assert_eq!(stranded_counts(&conn, &me).unwrap().get(&no), None);

        // And with scope set to "all" nothing is stranded anywhere.
        set_mode(&conn, &me, Mode::All).unwrap();
        assert!(stranded_counts(&conn, &me).unwrap().is_empty());
    }

    /// Creating a playlist in selective mode can't make it disappear: it's
    /// born with no scope row and no files, which is exactly what the tree
    /// hides.
    #[test]
    fn a_playlist_created_here_starts_selected() {
        let conn = mem();
        let me = db::this_device_uid(&conn).unwrap();
        set_mode(&conn, &me, Mode::Selected).unwrap();
        let id = db::create_playlist(&conn, "New", "playlist", None).unwrap();
        let uid: String = conn
            .query_row("SELECT uid FROM playlists WHERE id = ?1", [id], |r| r.get(0))
            .unwrap();

        assert!(!scope_playlists(&conn, &me).unwrap().unwrap().contains(&uid));
        select_new_local(&conn, id).unwrap();
        assert!(scope_playlists(&conn, &me).unwrap().unwrap().contains(&uid));

        // With scope set to "all" it writes nothing: there's nothing to mark.
        let other = mem();
        let them = db::this_device_uid(&other).unwrap();
        let id = db::create_playlist(&other, "New", "playlist", None).unwrap();
        select_new_local(&other, id).unwrap();
        assert!(entries(&other).unwrap().is_empty());
        assert!(scope_playlists(&other, &them).unwrap().is_none());
    }

    /// Hashing the trash is the most expensive thing here and it runs on
    /// every scope change. The cache has to survive between calls, and it
    /// has to let go of the old hash if the file changed.
    #[test]
    fn trash_hashes_survive_between_calls_but_not_a_changed_file() {
        let dir = std::env::temp_dir().join(format!(
            "sway-trash-cache-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("track.mp3");
        std::fs::write(&f, b"some bytes").unwrap();
        let real = crate::hashing::hash_file(&f).unwrap();

        let h1 = trash_hash(&f, 10, 111).unwrap();
        assert_eq!(h1, real);

        // Second round with the same key: comes from the cache. The file is
        // deleted to prove it — if it rehashed, there'd be nothing to hash.
        std::fs::remove_file(&f).unwrap();
        assert_eq!(trash_hash(&f, 10, 111).as_deref(), Some(real.as_str()));

        // A different date is a different file: the cache doesn't apply and
        // there's nothing to read.
        assert!(trash_hash(&f, 10, 222).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The hard requirement of this whole phase: freeing up space can't
    /// destroy the last copy of anything.
    #[test]
    fn a_track_nobody_else_has_is_never_evictable() {
        let conn = mem();
        let me = db::this_device_uid(&conn).unwrap();
        let dir = std::env::temp_dir();
        conn.execute(
            "INSERT INTO tracks (path, uid, content_hash, size_bytes, local_state)
             VALUES (?1, 'only-here', 'h1', 100, 'present')",
            [dir.join("x.flac").to_string_lossy()],
        )
        .unwrap();
        set_mode(&conn, &me, Mode::Selected).unwrap();

        // Outside scope, but nobody else has it: not offered.
        assert!(evictable(&conn, &dir).unwrap().is_empty());

        // With a confirmed replica on another device, it is.
        note_replicas(&conn, "other", &["h1".to_string()]).unwrap();
        assert_eq!(evictable(&conn, &dir).unwrap().len(), 1);

        // And a replica "on myself" doesn't count as a backup.
        conn.execute("DELETE FROM blob_replicas", []).unwrap();
        note_replicas(&conn, &me, &["h1".to_string()]).unwrap();
        assert!(evictable(&conn, &dir).unwrap().is_empty());
    }
}
