//! Applying metadata, playlist, and folder changes (Phase 5.5).
//!
//! 5.4 moved files; this moves the organization: names, folder hierarchy,
//! order, and which track is in which playlist.
//!
//! Rules, all with the same bias — when in doubt, keep:
//!
//! - **Metadata: the newest wins** (`updated_at`). No ties: if they're
//!   equal nothing is touched, so syncing twice in a row doesn't change
//!   anything the second time.
//! - **Playlists: the newest wins**, but only for name, parent, and order.
//!   A playlist only disappears with an explicit tombstone.
//! - **Memberships: union.** A track enters a playlist if any device put
//!   it there; it leaves only with an explicit tombstone. Adding beats a
//!   concurrent removal, which is the right bias when what's at stake is
//!   losing music from a set.
//!
//! - **Deletes: always applied**, and the file goes to the library trash,
//!   not into the void. The tombstone is saved regardless: it's what
//!   prevents giving back to the other side what it already removed.
//!
//!   Until Phase 6.4 there was a per-device policy to ignore or queue
//!   them. It was removed because it couldn't deliver what it promised: it
//!   filtered by who passed you the tombstone, not by who had deleted, so
//!   with three devices a delete you rejected on the phone still got in
//!   through the laptop. What actually protects you is the trash, which
//!   filters nothing.

use crate::manifest::{Manifest, Membership, PlaylistEntry, ScopeEntry, DeviceSync, TrackEntry, Tombstone};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};


/// Set of changes ready to apply on the other side.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Changes {
    pub tracks: Vec<TrackEntry>,
    pub playlists: Vec<PlaylistEntry>,
    pub memberships: Vec<Membership>,
    pub tombstones: Vec<Tombstone>,
    /// Selective scope (Phase 5.7). `default` so it can still talk to an
    /// older version without breaking.
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
    /// Things deleted by incoming tombstones (Phase 5.6).
    pub deleted: usize,
    /// Selective scope rows applied (Phase 5.7).
    #[serde(default)]
    pub scope: usize,
}

impl Applied {
    pub fn total(&self) -> usize {
        self.tracks + self.playlists + self.memberships + self.deleted + self.scope
    }
}

/// What `local` has that `remote` is missing or has gone stale on. This is
/// what needs to be sent so it's caught up.
/// Indexes of the other side's manifest. Built once and queried thousands
/// of times: with `find`/`any` over the lists, comparing two libraries of
/// twenty thousand tracks would mean hundreds of millions of string
/// comparisons — and this runs twice per sync, once per direction.
struct Index<'a> {
    tracks: std::collections::HashMap<&'a str, &'a TrackEntry>,
    playlists: std::collections::HashMap<&'a str, &'a PlaylistEntry>,
    memberships: std::collections::HashMap<String, &'a Membership>,
    /// (entity, uid) -> when it was deleted.
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
            // Only if ours is strictly newer: with `>=` everything would be
            // resent on every sync without anything actually changing.
            Some(r) => t.updated_at > r.updated_at,
            // If they don't have it, the row travels with the file (5.4).
            // Sending metadata for a track whose file isn't there would
            // create a ghost entry in the other side's library.
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

    // Memberships travel for two different reasons: because the other side
    // is missing the pair (union), or because the ORDER changed. Without
    // the second reason, a reorder never propagated: the pair already
    // existed on both sides and nobody ever resent it.
    let mine_pl: std::collections::HashMap<&str, i64> = local
        .playlists
        .iter()
        .map(|p| (p.uid.as_str(), p.updated_at))
        .collect();
    for m in &local.memberships {
        let key = format!("{}:{}", m.playlist_uid, m.track_uid);
        // A tombstone from the other side only wins if it's LATER than the
        // add. Otherwise, putting a song back into a playlist would undo
        // itself: the old delete would beat the new add, forever.
        if let Some(deleted_at) = they.tombstones.get(&("playlist_track", key.as_str())) {
            if *deleted_at >= m.added_at {
                continue;
            }
        }
        match they.memberships.get(&key) {
            None => out.memberships.push(m.clone()),
            // Order belongs to whoever touched the playlist most recently.
            // Memberships have no clock of their own; the playlist does.
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

    // Scope: send whatever the other side is missing or has gone stale on.
    // It travels in both directions like any other replicated data — every
    // device can edit everyone's scope, including its own.
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

/// The tombstone's `deleted_at`, if there is one.
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

/// Applies the received changes, deletes included.
///
/// A delete is always applied. There used to be a per-device policy that
/// allowed ignoring or queuing them for confirmation, removed in Phase 6.4
/// because it couldn't deliver what it promised: it filtered by **who
/// passed you** the tombstone, not by who deleted. With three devices, a
/// delete this one rejected on the phone still got in through the laptop,
/// which had accepted it. With a server always on, this stopped being a
/// rare case and became the rule.
///
/// What protects against an accidental delete is the trash, which filters
/// nothing and always works: the file goes to `trash` and stays
/// there for 30 days.
pub fn apply(
    conn: &Connection,
    changes: &Changes,
    music_dir: &std::path::Path,
) -> rusqlite::Result<Applied> {
    let mut applied = Applied::default();

    // --- Track metadata ------------------------------------------------
    for t in &changes.tracks {
        let local: Option<(i64, i64)> = conn
            .query_row(
                "SELECT id, updated_at FROM tracks WHERE uid = ?1",
                [&t.uid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        // A track that isn't here doesn't get created from loose metadata:
        // its row arrives together with the file (5.4). Otherwise there'd
        // be an entry left over that can't be played.
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

    // --- Playlists and folders ------------------------------------------
    //
    // Two passes: first everything is created (with no parent), then it's
    // hooked up. They can arrive in any order and a folder can arrive
    // after its children; resolving the parent in a single pass would
    // leave dangling nodes.
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
    // Second pass: now the parents actually exist.
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

    // --- Memberships -----------------------------------------------------
    for m in &changes.memberships {
        let key = format!("{}:{}", m.playlist_uid, m.track_uid);
        // Same as above: the local tombstone only wins if it's later than
        // the incoming add.
        if let Some(deleted_at) = tombstone_at(conn, "playlist_track", &key) {
            if deleted_at >= m.added_at {
                continue;
            }
            // The add is newer: the old delete is stale and in the way.
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
        // If the track hasn't reached this device yet, the membership is
        // ignored: the next sync brings it, once the track exists.
        let Some((pid, tid)) = ids else { continue };
        // `DO UPDATE` and not `OR IGNORE`: a pair that already exists can
        // arrive with a different rank, which is how a reorder travels.
        let n = conn.execute(
            "INSERT INTO playlist_tracks (playlist_id, track_id, rank, added_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(playlist_id, track_id) DO UPDATE SET rank = excluded.rank
             WHERE playlist_tracks.rank <> excluded.rank",
            rusqlite::params![pid, tid, m.rank, m.added_at],
        )?;
        applied.memberships += n;
    }

    // --- Selective scope (Phase 5.7) --------------------------------------
    //
    // Replicated data like anything else, LWW per row: the phone's scope
    // gets edited from the PC and vice versa.
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

    // --- Tombstones (Phase 5.6) -------------------------------------------
    //
    // Always saved, even when the policy doesn't delete: this is what
    // prevents this device from giving back to the other side what it
    // already deleted. Whether to apply them or not is a separate decision.
    for t in &changes.tombstones {
        conn.execute(
            "INSERT OR IGNORE INTO tombstones (entity, uid, deleted_at, device_uid)
             VALUES (?1, ?2, ?3, '')",
            rusqlite::params![t.entity, t.uid, t.deleted_at],
        )?;
        applied.deleted += apply_tombstone(conn, music_dir, t)?;
    }

    Ok(applied)
}

/// Applies a delete. Returns 1 if it deleted something, 0 if there was
/// nothing to delete.
///
/// The audio file **is not destroyed**: it goes to the library trash,
/// where it survives 30 days. A local delete you did while watching the
/// screen; one arriving over the network had better be recoverable.
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
            // Only what lives in the managed folder gets touched. A legacy
            // file from outside isn't ours to move.
            if path.starts_with(music_dir) && path.exists() {
                match crate::trash::move_to_trash(music_dir, path) {
                    Ok(dest) => log::info!("[sync] moved to trash: {}", dest.display()),
                    Err(e) => {
                        // If the file couldn't be moved, the row stays: a
                        // row with no file is worse than a delete that
                        // didn't apply and gets retried on the next sync.
                        log::warn!("[sync] could not move {} to trash: {e}", path.display());
                        return Ok(0);
                    }
                }
            }
            // The CASCADE removes it from every playlist.
            conn.execute("DELETE FROM tracks WHERE id = ?1", [id])?;
            Ok(1)
        }
        "playlist" => {
            let n = conn.execute("DELETE FROM playlists WHERE uid = ?1", [&t.uid])?;
            Ok(n)
        }
        "playlist_track" => {
            let Some((pl, tr)) = t.uid.split_once(':') else { return Ok(0) };
            // Only if the delete is later than the local add. If it was
            // added again after that, that add is the last word.
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
        add_track(&conn, "t1", "old", 100);
        let changes = Changes {
            tracks: vec![entry("t1", "new", 500)],
            ..Default::default()
        };
        assert_eq!(apply(&conn, &changes, std::path::Path::new("/nowhere")).unwrap().tracks, 1);
        let title: String = conn
            .query_row("SELECT title FROM tracks WHERE uid = 't1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(title, "new");

        // An older change overwrites nothing.
        let old = Changes {
            tracks: vec![entry("t1", "previous", 200)],
            ..Default::default()
        };
        assert_eq!(apply(&conn, &old, std::path::Path::new("/nowhere")).unwrap().tracks, 0);
        let title: String = conn
            .query_row("SELECT title FROM tracks WHERE uid = 't1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(title, "new");
    }

    /// Applying the same thing twice can't change anything the second time:
    /// otherwise every sync would count as pending work forever.
    #[test]
    fn applying_twice_is_a_no_op() {
        let conn = mem();
        add_track(&conn, "t1", "old", 100);
        let changes = Changes {
            tracks: vec![entry("t1", "new", 500)],
            playlists: vec![pl("p1", "Sets", None, 10)],
            memberships: vec![Membership {
                playlist_uid: "p1".into(),
                track_uid: "t1".into(),
                rank: "V".into(),
                added_at: 0,
            }],
            ..Default::default()
        };
        let first = apply(&conn, &changes, std::path::Path::new("/nowhere")).unwrap();
        assert!(first.total() > 0);
        let second = apply(&conn, &changes, std::path::Path::new("/nowhere")).unwrap();
        assert_eq!(second.total(), 0, "the second pass must not change anything");
    }

    /// Folders can arrive after their children: the hierarchy still has to
    /// come out right.
    #[test]
    fn hierarchy_resolves_regardless_of_arrival_order() {
        let conn = mem();
        let changes = Changes {
            // The child first, the parent after.
            playlists: vec![
                pl("p1", "Warmup", Some("f1"), 10),
                pl("f1", "Electronica", None, 10),
            ],
            ..Default::default()
        };
        apply(&conn, &changes, std::path::Path::new("/nowhere")).unwrap();

        let (child_parent, folder_id): (Option<i64>, i64) = (
            conn.query_row("SELECT parent_id FROM playlists WHERE uid = 'p1'", [], |r| r.get(0))
                .unwrap(),
            conn.query_row("SELECT id FROM playlists WHERE uid = 'f1'", [], |r| r.get(0))
                .unwrap(),
        );
        assert_eq!(child_parent, Some(folder_id));
    }

    /// A playlist deleted here doesn't come back just because the other
    /// side still has it.
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
        assert_eq!(apply(&conn, &changes, std::path::Path::new("/nowhere")).unwrap().playlists, 0);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM playlists", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    /// Removing a track from a playlist only propagates with a tombstone;
    /// without one, the union puts it back (adding beats a concurrent
    /// removal).
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
            std::path::Path::new("/nowhere"))
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
        assert_eq!(apply(&conn, &member, std::path::Path::new("/nowhere")).unwrap().memberships, 1);

        // Removed here, with a record of it.
        conn.execute("DELETE FROM playlist_tracks", []).unwrap();
        conn.execute(
            "INSERT INTO tombstones (entity, uid, deleted_at) VALUES ('playlist_track','p1:t1',999)",
            [],
        )
        .unwrap();
        assert_eq!(apply(&conn, &member, std::path::Path::new("/nowhere")).unwrap().memberships, 0, "must not come back");
    }

    /// A membership for a track that hasn't arrived yet is skipped without
    /// breaking anything; the next sync brings it once the file is there.
    #[test]
    fn membership_of_an_unknown_track_is_skipped() {
        let conn = mem();
        let changes = Changes {
            playlists: vec![pl("p1", "Sets", None, 10)],
            memberships: vec![Membership {
                playlist_uid: "p1".into(),
                track_uid: "not-yet".into(),
                rank: "V".into(),
                added_at: 0,
            }],
            ..Default::default()
        };
        let applied = apply(&conn, &changes, std::path::Path::new("/nowhere")).unwrap();
        assert_eq!(applied.playlists, 1);
        assert_eq!(applied.memberships, 0);
    }

    /// Metadata for a track whose file isn't there doesn't create a ghost
    /// entry: the row arrives together with the file.
    #[test]
    fn metadata_for_an_unknown_track_does_not_create_a_row() {
        let conn = mem();
        let changes = Changes {
            tracks: vec![entry("never-seen", "x", 10)],
            ..Default::default()
        };
        assert_eq!(apply(&conn, &changes, std::path::Path::new("/nowhere")).unwrap().tracks, 0);
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

    /// A delete that arrives over the network removes the track from the
    /// library, but the file goes to the trash: recoverable, not destroyed.
    #[test]
    fn a_propagated_delete_moves_the_file_to_the_trash() {
        let music = tmp_music("del");
        let conn = mem();
        let f = music.join("deleted.flac");
        std::fs::write(&f, b"audio").unwrap();
        add_track_at(&conn, "t1", &f);

        let changes = Changes {
            tombstones: vec![tomb("track", "t1")],
            ..Default::default()
        };
        let applied = apply(&conn, &changes, &music).unwrap();

        assert_eq!(applied.deleted, 1);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "leaves the library");
        assert!(!f.exists(), "and the managed folder");
        let recoverable = std::fs::read_dir(crate::trash::trash_dir(&music))
            .unwrap()
            .flatten()
            .any(|e| std::fs::read(e.path()).unwrap() == b"audio");
        assert!(recoverable, "but still exists in the trash");
        std::fs::remove_dir_all(&music).ok();
    }

    /// Scope travels like any other replicated data: I edit it here for
    /// the phone and the phone finds out.
    #[test]
    fn scope_rows_travel_and_the_newest_wins() {
        let mut local = empty_manifest();
        local.scopes.push(ScopeEntry {
            device_uid: "phone".into(),
            playlist_uid: "sets".into(),
            selected: true,
            updated_at: 500,
        });
        local.device_sync.push(DeviceSync {
            device_uid: "phone".into(),
            mode: "selected".into(),
            direction: "both".into(),
            updated_at: 500,
        });
        let mut remote = empty_manifest();
        remote.scopes.push(ScopeEntry {
            device_uid: "phone".into(),
            playlist_uid: "sets".into(),
            selected: false,
            updated_at: 100,
        });

        let out = changes_for_peer(&local, &remote);
        assert_eq!(out.scopes.len(), 1, "mine is newer: it travels");
        assert_eq!(out.device_sync.len(), 1);

        // And nothing comes back the other way: theirs went stale.
        assert!(changes_for_peer(&remote, &local).scopes.is_empty());

        let conn = mem();
        let applied = apply(&conn, &out, std::path::Path::new("/nowhere")).unwrap();
        assert_eq!(applied.scope, 2);
        let s = crate::scope::get(&conn, "phone").unwrap();
        assert_eq!(s.mode, crate::scope::Mode::Selected);
        assert!(s.selected.contains("sets"));
    }

    /// Deleting a playlist can't take its music with it.
    #[test]
    fn deleting_a_playlist_does_not_delete_its_tracks() {
        let music = tmp_music("pl");
        let conn = mem();
        let f = music.join("survives.flac");
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
            &music)
        .unwrap();

        apply(
            &conn,
            &Changes {
                tombstones: vec![tomb("playlist", "p1")],
                ..Default::default()
            },
            &music)
        .unwrap();

        let playlists: i64 = conn
            .query_row("SELECT COUNT(*) FROM playlists", [], |r| r.get(0))
            .unwrap();
        let tracks: i64 = conn
            .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(playlists, 0);
        assert_eq!(tracks, 1, "the track stays in the library");
        assert!(f.exists(), "and so does its file");
        std::fs::remove_dir_all(&music).ok();
    }

    /// A file outside the managed folder (legacy) isn't ours to move: it's
    /// removed from the library and the file stays where it is.
    #[test]
    fn a_file_outside_the_managed_folder_is_left_alone() {
        let music = tmp_music("legacy-managed");
        let elsewhere = tmp_music("legacy-outside");
        let conn = mem();
        let f = elsewhere.join("foreign.flac");
        std::fs::write(&f, b"audio").unwrap();
        add_track_at(&conn, "t1", &f);

        apply(
            &conn,
            &Changes {
                tombstones: vec![tomb("track", "t1")],
                ..Default::default()
            },
            &music)
        .unwrap();

        assert!(f.exists(), "the foreign file isn't touched");
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

    /// Reordering neither adds nor removes anything: the pair already
    /// exists on both sides and only its rank changes. Without this,
    /// moving a song within a playlist never propagated — and the order of
    /// a set is kind of the point.
    #[test]
    fn reordering_travels_when_the_playlist_is_newer() {
        let reordered = manifest_with(500, "b");
        let stale = manifest_with(100, "a");

        let c = changes_for_peer(&reordered, &stale);
        assert_eq!(c.memberships.len(), 1, "the new rank has to travel");
        assert_eq!(c.memberships[0].rank, "b");

        // And not in the other direction: whoever touched it last wins.
        let c = changes_for_peer(&stale, &reordered);
        assert!(c.memberships.is_empty());
    }

    /// Same order on both sides: nothing to send, no matter how different
    /// the playlist's dates are.
    #[test]
    fn an_unchanged_order_is_not_resent() {
        let a = manifest_with(500, "a");
        let b = manifest_with(100, "a");
        assert!(changes_for_peer(&a, &b).memberships.is_empty());
    }

    /// And when applying, a pair that already exists has to end up with the
    /// new rank.
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
        let music = std::path::Path::new("/nowhere");
        apply(&conn, &base, music).unwrap();

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
            apply(&conn, &reorder, music)
                .unwrap()
                .memberships,
            1
        );
        let rank: String = conn
            .query_row("SELECT rank FROM playlist_tracks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rank, "z");

        // And applying it again doesn't count as work.
        assert_eq!(
            apply(&conn, &reorder, music)
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

    /// Re-adding a song to a playlist it had been removed from. The
    /// tombstone from back then can't beat the new add: if it did, adding
    /// it back would undo itself on the next sync — which is exactly what
    /// used to happen.
    #[test]
    fn re_adding_a_track_beats_an_older_removal() {
        let music = std::path::Path::new("/nowhere");
        let conn = mem();
        add_track(&conn, "t1", "x", 1);
        apply(
            &conn,
            &Changes {
                playlists: vec![pl("p1", "Sets", None, 10)],
                ..Default::default()
            },
            music)
        .unwrap();

        // Removed a while ago...
        conn.execute(
            "INSERT INTO tombstones (entity, uid, deleted_at) VALUES ('playlist_track','p1:t1',100)",
            [],
        )
        .unwrap();

        // ...and now a LATER add arrives.
        let re_add = Changes {
            memberships: vec![member("V", 500)],
            ..Default::default()
        };
        assert_eq!(apply(&conn, &re_add, music).unwrap().memberships, 1);

        // And the old tombstone gets cleared, so it doesn't get in the way again.
        assert!(!has_tombstone(&conn, "playlist_track", "p1:t1"));

        // A LATER tombstone does win.
        let removal = Changes {
            tombstones: vec![Tombstone {
                entity: "playlist_track".into(),
                uid: "p1:t1".into(),
                deleted_at: 900,
            }],
            ..Default::default()
        };
        assert_eq!(apply(&conn, &removal, music).unwrap().deleted, 1);
    }

    /// And a delete that arrives stale can't remove something added later.
    #[test]
    fn an_older_removal_does_not_undo_a_newer_add() {
        let music = std::path::Path::new("/nowhere");
        let conn = mem();
        add_track(&conn, "t1", "x", 1);
        apply(
            &conn,
            &Changes {
                playlists: vec![pl("p1", "Sets", None, 10)],
                memberships: vec![member("V", 800)],
                ..Default::default()
            },
            music)
        .unwrap();

        let stale = Changes {
            tombstones: vec![Tombstone {
                entity: "playlist_track".into(),
                uid: "p1:t1".into(),
                deleted_at: 200,
            }],
            ..Default::default()
        };
        assert_eq!(apply(&conn, &stale, music).unwrap().deleted, 0);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM playlist_tracks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "the song stays");
    }

    /// The same criterion on the side deciding what to send: a new add
    /// isn't skipped because of an older tombstone from the other side.
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

        // But if the other side's tombstone is later, it isn't sent.
        remote.tombstones[0].deleted_at = 900;
        assert!(changes_for_peer(&local, &remote).memberships.is_empty());
    }

    #[test]
    fn changes_for_peer_only_includes_what_is_newer_or_missing() {
        let local = Manifest {
            device_uid: "a".into(),
            tracks: vec![entry("t1", "new", 500), entry("t2", "same", 100)],
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
            tracks: vec![entry("t1", "old", 100), entry("t2", "same", 100)],
            playlists: vec![],
            memberships: vec![],
            tombstones: vec![],
            ..Default::default()
        };
        let c = changes_for_peer(&local, &remote);
        assert_eq!(c.tracks.len(), 1, "only t1, which is newer");
        assert_eq!(c.tracks[0].uid, "t1");
        assert_eq!(c.playlists.len(), 1);
        assert_eq!(c.memberships.len(), 1);
    }
}
