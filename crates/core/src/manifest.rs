//! Library inventory and sync plan calculation (Phase 5.3).
//!
//! Nothing gets written here. Both sides send what they have and the plan
//! computes what should happen; only 5.4 actually executes it. Keeping it
//! separate like this is deliberate: the merge is the part where a bug can
//! delete music, and it can be tested in full — and eyeballed in the UI —
//! before it gets anywhere near a file.
//!
//! `plan()` is a pure function over two manifests. All the hard decisions
//! live there and are tested without a network, a DB, or any devices.

use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};

// ---------------------------------------------------------------------------
// Inventory compression
// ---------------------------------------------------------------------------

/// Cap on what is accepted for decompression.
///
/// A tiny compressed `Vec` can expand to gigabytes if it was crafted by
/// someone hoping to run the other side out of memory. The limit is the same
/// one the channel already had for a whole message (see `wire.rs`): an
/// inventory bigger than this is a problem even uncompressed.
const MAX_INFLATED: u64 = 64 * 1024 * 1024;

/// Compresses the inventory.
///
/// The inventory is JSON and about as compressible as it gets: the same keys
/// repeated in every row, uuids and hashes in hex. This isn't a minor
/// efficiency detail — it's what travels **whole** on every comparison,
/// whether anything changed or not, and out of a phone's data plan.
pub fn squeeze(json: &[u8]) -> Result<Vec<u8>> {
    // Fast compression, not maximum: the gap between the two extremes is a
    // few percentage points on something that's already ten times smaller,
    // and the serving side can be a Raspberry Pi.
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    enc.write_all(json)?;
    Ok(enc.finish()?)
}

/// Turns it back into JSON.
pub fn expand(gz: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(gz)
        .take(MAX_INFLATED)
        .read_to_end(&mut out)?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Inventory
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackEntry {
    pub uid: String,
    /// `None` while the hash backfill hasn't reached this track yet. Without
    /// a hash the file can't be requested or verified, so it doesn't
    /// participate.
    pub hash: Option<String>,
    pub size: i64,
    pub filename: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub duration_ms: i64,
    pub bpm: Option<i64>,
    pub updated_at: i64,
    /// The file is on this device (wasn't evacuated by selective sync).
    pub present: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistEntry {
    pub uid: String,
    pub name: String,
    pub kind: String,
    pub parent_uid: Option<String>,
    pub rank: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Membership {
    pub playlist_uid: String,
    pub track_uid: String,
    pub rank: String,
    /// When the track was added to the playlist. Compared against the
    /// `deleted_at` of the tombstone for the same pair: the most recent one
    /// wins.
    #[serde(default)]
    pub added_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tombstone {
    pub entity: String,
    pub uid: String,
    pub deleted_at: i64,
}

/// A playlist marked (or unmarked) for a device. Travels because scope can
/// be edited from either side — see `scope.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopeEntry {
    pub device_uid: String,
    pub playlist_uid: String,
    pub selected: bool,
    pub updated_at: i64,
}

/// What a device does: which direction it syncs and whether it takes the
/// whole library or just what's marked. It's a property of the device, not
/// of the link — between two devices, A → B happens only if A sends and B
/// receives, and both sides hold both rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSync {
    pub device_uid: String,
    /// all | selected
    pub mode: String,
    /// both | send | receive | off
    #[serde(default = "default_direction")]
    pub direction: String,
    pub updated_at: i64,
}

fn default_direction() -> String {
    "both".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub device_uid: String,
    pub tracks: Vec<TrackEntry>,
    pub playlists: Vec<PlaylistEntry>,
    pub memberships: Vec<Membership>,
    pub tombstones: Vec<Tombstone>,
    /// Scope of all known devices (Phase 5.7). `default` so a manifest from
    /// an earlier version can still be read without breaking.
    #[serde(default)]
    pub scopes: Vec<ScopeEntry>,
    #[serde(default)]
    pub device_sync: Vec<DeviceSync>,
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileTransfer {
    pub track_uid: String,
    pub hash: String,
    pub filename: String,
    pub size: i64,
}

/// What would happen if a sync ran right now. The counts are what gets shown
/// in the UI before anything is enabled.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    /// Files the peer has and we're missing.
    pub pull_files: Vec<FileTransfer>,
    /// Files we have and the peer is missing.
    pub push_files: Vec<FileTransfer>,
    /// Tracks whose metadata is newer on the other side (and vice versa).
    pub pull_meta: usize,
    pub push_meta: usize,
    /// Playlists/folders that don't exist on the other side (and vice versa).
    pub pull_playlists: usize,
    pub push_playlists: usize,
    /// Tracks added to playlists that don't appear on the other side.
    pub pull_memberships: usize,
    pub push_memberships: usize,
    /// Deletions that would need to be applied here / there.
    pub deletes_in: usize,
    pub deletes_out: usize,
    /// Tracks that don't participate because they don't have a computed hash
    /// yet.
    pub unhashed: usize,
    /// Files that are NOT pulled / NOT pushed because they fell outside the
    /// selective scope of the device that would receive them (Phase 5.7).
    /// These aren't pending work: they're selective sync working as
    /// intended. Shown anyway, because "300 files missing and the plan says
    /// zero" needs an explanation.
    pub out_of_scope_in: usize,
    pub out_of_scope_out: usize,
}

impl Plan {
    pub fn bytes_in(&self) -> i64 {
        self.pull_files.iter().map(|f| f.size).sum()
    }
    pub fn bytes_out(&self) -> i64 {
        self.push_files.iter().map(|f| f.size).sum()
    }
    pub fn is_empty(&self) -> bool {
        self.pull_files.is_empty()
            && self.push_files.is_empty()
            && self.pull_meta == 0
            && self.push_meta == 0
            && self.pull_playlists == 0
            && self.push_playlists == 0
            && self.pull_memberships == 0
            && self.push_memberships == 0
            && self.deletes_in == 0
            && self.deletes_out == 0
    }
}

fn tombstone_keys(m: &Manifest) -> HashSet<(&str, &str)> {
    m.tombstones
        .iter()
        .map(|t| (t.entity.as_str(), t.uid.as_str()))
        .collect()
}

fn pair_key(playlist_uid: &str, track_uid: &str) -> String {
    format!("{playlist_uid}:{track_uid}")
}

/// The same track (same uid) with different bytes on each device: a
/// different encoding, a different edit, a tagger that rewrote the file.
///
/// Only one can stick around, and **both sides have to pick the same one**.
/// If each side pulls the other's, they'd swap it back and forth on every
/// sync forever: A ends up with B's copy, B with A's, and the next round
/// flips again. That's why the rule can't be "pull whatever you're missing"
/// but a comparison that comes out identical on both ends.
///
/// The newer one wins; on a tie, the hash breaks it. The tiebreaker is
/// arbitrary, but the only thing that matters is that it's the same on both
/// sides.
fn wins(a: &TrackEntry, b: &TrackEntry) -> bool {
    (a.updated_at, a.hash.as_deref()) > (b.updated_at, b.hash.as_deref())
}

/// Computes what a sync between `local` and `remote` would do. Touches
/// nothing.
///
/// Biases, all pointing the same way — when in doubt, keep:
/// - A track whose `uid` was deleted here is NOT pulled back. Without this,
///   every sync would resurrect what was deleted and there'd be no way to
///   actually remove anything from the library.
/// - A file counts as present if its **content** (hash) matches, not its
///   uid: the same MP3 imported separately on two devices has different
///   uids, and transferring it again would burn bandwidth just to end up
///   with two identical copies.
/// - Memberships (track inside playlist) are unioned; removal only
///   propagates if there's an explicit tombstone.
/// - Selective scope filters **only files**: playlists, order, and metadata
///   travel in full regardless. A track outside of scope still shows up in
///   the other device's library, just without a file.
pub fn plan(local: &Manifest, remote: &Manifest) -> Plan {
    let mut p = Plan::default();

    // Scope resolved from the newest rows on both sides: during the window
    // where a change hasn't traveled yet, the manifests differ and the
    // decision has to go by the most recent row, not by our own.
    let all_entries = crate::scope::merge_entries(&local.scopes, &remote.scopes);
    let all_modes = crate::scope::merge_device_sync(&local.device_sync, &remote.device_sync);
    // Hierarchy and memberships are resolved over the union: a playlist that
    // only exists on one side still decides what gets included.
    let all_playlists: Vec<PlaylistEntry> = local
        .playlists
        .iter()
        .chain(remote.playlists.iter())
        .cloned()
        .collect();
    let all_members: Vec<Membership> = local
        .memberships
        .iter()
        .chain(remote.memberships.iter())
        .cloned()
        .collect();
    let scope_of = |device_uid: &str| {
        let s = crate::scope::from_entries(device_uid, &all_entries, &all_modes);
        crate::scope::tracks_in_scope(&all_playlists, &all_members, &s)
    };
    let local_scope = scope_of(&local.device_uid);
    let remote_scope = scope_of(&remote.device_uid);
    let wants = |scope: &Option<HashSet<String>>, uid: &str| match scope {
        None => true,
        Some(set) => set.contains(uid),
    };

    let local_hashes: HashSet<&str> = local
        .tracks
        .iter()
        .filter(|t| t.present)
        .filter_map(|t| t.hash.as_deref())
        .collect();
    let remote_hashes: HashSet<&str> = remote
        .tracks
        .iter()
        .filter(|t| t.present)
        .filter_map(|t| t.hash.as_deref())
        .collect();
    let local_by_uid: HashMap<&str, &TrackEntry> =
        local.tracks.iter().map(|t| (t.uid.as_str(), t)).collect();
    let remote_by_uid: HashMap<&str, &TrackEntry> =
        remote.tracks.iter().map(|t| (t.uid.as_str(), t)).collect();

    // Tombstones are queried once per track, per playlist, and per
    // membership. Scanning the list on every lookup makes the sync
    // quadratic: with 20 thousand tracks and a handful of deletions that's
    // hundreds of millions of string comparisons, twice per run.
    let local_tombs: HashSet<(&str, &str)> = tombstone_keys(local);
    let remote_tombs: HashSet<(&str, &str)> = tombstone_keys(remote);
    let tombstoned = |set: &HashSet<(&str, &str)>, entity: &str, uid: &str| {
        set.contains(&(entity, uid))
    };

    p.unhashed = local.tracks.iter().filter(|t| t.hash.is_none()).count();

    // --- Files ----------------------------------------------------------
    for t in remote.tracks.iter().filter(|t| t.present) {
        let Some(hash) = t.hash.as_deref() else { continue };
        if local_hashes.contains(hash) || tombstoned(&local_tombs, "track", &t.uid) {
            continue;
        }
        if let Some(l) = local_by_uid.get(t.uid.as_str()).filter(|l| l.present) {
            // That track is already here but doesn't have a computed hash
            // yet: there's no way to tell if it's the same content.
            // Pulling it "just in case" means transferring it in full on
            // EVERY sync until the backfill catches up — which is exactly
            // what used to happen: the same track traveling back and forth
            // while present on both sides. So it waits for the hash.
            if l.hash.is_none() {
                continue;
            }
            // It's here with different bytes: only pulled if the remote
            // one wins the tiebreak (see `wins`), or the two would swap the
            // file back and forth forever.
            if l.hash.as_deref() != Some(hash) && !wins(t, l) {
                continue;
            }
        }
        if !wants(&local_scope, &t.uid) {
            p.out_of_scope_in += 1;
            continue;
        }
        p.pull_files.push(FileTransfer {
            track_uid: t.uid.clone(),
            hash: hash.to_string(),
            filename: t.filename.clone(),
            size: t.size,
        });
    }
    for t in local.tracks.iter().filter(|t| t.present) {
        let Some(hash) = t.hash.as_deref() else { continue };
        if remote_hashes.contains(hash) || tombstoned(&remote_tombs, "track", &t.uid) {
            continue;
        }
        // Mirror of the above, seen from the other side: nothing gets sent
        // that's already there under the same uid, unless ours wins the
        // tiebreak. Both sides run the same comparison and land on the same
        // winner, so the file moves once and in a single direction.
        if let Some(r) = remote_by_uid.get(t.uid.as_str()).filter(|r| r.present) {
            if r.hash.is_none() {
                continue;
            }
            if r.hash.as_deref() != Some(hash) && !wins(t, r) {
                continue;
            }
        }
        if !wants(&remote_scope, &t.uid) {
            p.out_of_scope_out += 1;
            continue;
        }
        p.push_files.push(FileTransfer {
            track_uid: t.uid.clone(),
            hash: hash.to_string(),
            filename: t.filename.clone(),
            size: t.size,
        });
    }

    // --- Metadata (LWW per row; per-field detail arrives in 5.5) -----------
    for (uid, r) in remote_by_uid.iter() {
        if let Some(l) = local_by_uid.get(uid) {
            if r.updated_at > l.updated_at {
                p.pull_meta += 1;
            } else if l.updated_at > r.updated_at {
                p.push_meta += 1;
            }
        }
    }

    // --- Playlists ----------------------------------------------------------
    let local_pl: HashSet<&str> = local.playlists.iter().map(|p| p.uid.as_str()).collect();
    let remote_pl: HashSet<&str> = remote.playlists.iter().map(|p| p.uid.as_str()).collect();
    p.pull_playlists = remote
        .playlists
        .iter()
        .filter(|pl| !local_pl.contains(pl.uid.as_str()) && !tombstoned(&local_tombs, "playlist", &pl.uid))
        .count();
    p.push_playlists = local
        .playlists
        .iter()
        .filter(|pl| !remote_pl.contains(pl.uid.as_str()) && !tombstoned(&remote_tombs, "playlist", &pl.uid))
        .count();

    // --- Memberships ---------------------------------------------------------
    let local_pairs: HashSet<String> = local
        .memberships
        .iter()
        .map(|m| pair_key(&m.playlist_uid, &m.track_uid))
        .collect();
    let remote_pairs: HashSet<String> = remote
        .memberships
        .iter()
        .map(|m| pair_key(&m.playlist_uid, &m.track_uid))
        .collect();
    p.pull_memberships = remote
        .memberships
        .iter()
        .filter(|m| {
            let k = pair_key(&m.playlist_uid, &m.track_uid);
            !local_pairs.contains(&k) && !tombstoned(&local_tombs, "playlist_track", &k)
        })
        .count();
    p.push_memberships = local
        .memberships
        .iter()
        .filter(|m| {
            let k = pair_key(&m.playlist_uid, &m.track_uid);
            !remote_pairs.contains(&k) && !tombstoned(&remote_tombs, "playlist_track", &k)
        })
        .count();

    // --- Deletions -----------------------------------------------------------
    // A tombstone only counts if the thing it deletes still exists on the
    // other side. The sets are already built above: checking by scanning
    // the list for every tombstone made this quadratic.
    let exists_in = |uids: &HashMap<&str, &TrackEntry>,
                     pls: &HashSet<&str>,
                     pairs: &HashSet<String>,
                     entity: &str,
                     uid: &str| match entity {
        "track" => uids.contains_key(uid),
        "playlist" => pls.contains(uid),
        "playlist_track" => pairs.contains(uid),
        _ => false,
    };
    p.deletes_in = remote
        .tombstones
        .iter()
        .filter(|t| exists_in(&local_by_uid, &local_pl, &local_pairs, &t.entity, &t.uid))
        .count();
    p.deletes_out = local
        .tombstones
        .iter()
        .filter(|t| exists_in(&remote_by_uid, &remote_pl, &remote_pairs, &t.entity, &t.uid))
        .count();

    p
}

// ---------------------------------------------------------------------------
// Building from the DB
// ---------------------------------------------------------------------------

pub fn build(conn: &Connection) -> rusqlite::Result<Manifest> {
    let device_uid = crate::db::this_device_uid(conn)?;

    let tracks = {
        let mut stmt = conn.prepare(
            "SELECT uid, content_hash, COALESCE(size_bytes, 0), COALESCE(rel_path, ''),
                    title, artist, album, genre, duration_ms, bpm,
                    updated_at, local_state
             FROM tracks WHERE uid IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(TrackEntry {
                uid: r.get(0)?,
                hash: r.get(1)?,
                size: r.get(2)?,
                filename: r.get(3)?,
                title: r.get(4)?,
                artist: r.get(5)?,
                album: r.get(6)?,
                genre: r.get(7)?,
                duration_ms: r.get(8)?,
                bpm: r.get(9)?,
                updated_at: r.get(10)?,
                present: r.get::<_, String>(11)? == "present",
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    // `parent_uid` instead of `parent_id`: INTEGER ids are local and mean
    // nothing on the other side.
    let playlists = {
        let mut stmt = conn.prepare(
            "SELECT p.uid, p.name, p.kind, parent.uid, p.rank, p.updated_at
             FROM playlists p LEFT JOIN playlists parent ON parent.id = p.parent_id
             WHERE p.uid IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PlaylistEntry {
                uid: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                parent_uid: r.get(3)?,
                rank: r.get(4)?,
                updated_at: r.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let memberships = {
        let mut stmt = conn.prepare(
            "SELECT p.uid, t.uid, pt.rank, pt.added_at
             FROM playlist_tracks pt
             JOIN playlists p ON p.id = pt.playlist_id
             JOIN tracks t ON t.id = pt.track_id
             WHERE p.uid IS NOT NULL AND t.uid IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Membership {
                playlist_uid: r.get(0)?,
                track_uid: r.get(1)?,
                rank: r.get(2)?,
                added_at: r.get(3)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let tombstones = {
        let mut stmt = conn.prepare("SELECT entity, uid, deleted_at FROM tombstones")?;
        let rows = stmt.query_map([], |r| {
            Ok(Tombstone {
                entity: r.get(0)?,
                uid: r.get(1)?,
                deleted_at: r.get(2)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    Ok(Manifest {
        device_uid,
        tracks,
        playlists,
        memberships,
        tombstones,
        scopes: crate::scope::entries(conn)?,
        device_sync: crate::scope::all_device_sync(conn)?,
    })
}

#[cfg(test)]
mod compression {
    use super::*;

    #[test]
    fn what_gets_compressed_comes_back_the_same() {
        let json = br#"{"tracks":[{"uid":"a","title":"A track"}],"playlists":[]}"#;
        let gz = squeeze(json).unwrap();
        assert_eq!(expand(&gz).unwrap(), json);
    }

    /// An empty inventory also has to survive: it's what a freshly installed
    /// device sends, and it's exactly the case where a mistake would wipe
    /// out the other side's library.
    #[test]
    fn an_empty_inventory_survives() {
        let json = b"{}";
        assert_eq!(expand(&squeeze(json).unwrap()).unwrap(), json);
    }

    /// Garbage compressed data can't hang or crash the receiver: it has to
    /// return an error and nothing more.
    #[test]
    fn something_that_is_not_gzip_errors_without_panicking() {
        assert!(expand(b"this isn't gzip by any stretch").is_err());
    }
}

#[cfg(test)]
mod weight {
    use super::*;

    /// How much the inventory that travels on EVERY sync weighs, whether
    /// anything changed or not. This isn't an assertion, it's a measurement:
    /// run it with `cargo test -p sway-core -- --ignored --nocapture weight`.
    ///
    /// It matters because the inventory doesn't depend on what changed but
    /// on what exists: a library sitting still costs exactly the same as one
    /// that moved entirely. That's what makes the periodic pass scale with
    /// the size of the library instead of with the amount of new activity.
    #[test]
    #[ignore]
    fn how_much_the_inventory_weighs() {
        for (tracks, per_track) in [(100usize, 3usize), (1_000, 3), (5_000, 3)] {
            let m = synthetic(tracks, per_track);
            let json = serde_json::to_vec(&m).unwrap();
            let gz = crate::manifest::squeeze(&json).unwrap();
            println!(
                "{tracks} tracks, {} memberships -> {:.2} MB raw, {:.2} MB compressed ({:.1}x)",
                m.memberships.len(),
                json.len() as f64 / (1024.0 * 1024.0),
                gz.len() as f64 / (1024.0 * 1024.0),
                json.len() as f64 / gz.len() as f64,
            );
        }
    }

    /// A fake library with realistic measurements: UUID uids, blake3 hex
    /// hashes, and titles and filenames of typical length.
    fn synthetic(tracks: usize, playlists_per_track: usize) -> Manifest {
        let uid = |n: usize| format!("{n:08x}-1111-4222-8333-444455556666");
        let hash = |n: usize| format!("{:064x}", n);
        Manifest {
            device_uid: uid(0),
            tracks: (0..tracks)
                .map(|i| TrackEntry {
                    uid: uid(i),
                    hash: Some(hash(i)),
                    size: 9_325_265,
                    filename: format!("Artist {i} - A Pretty Long Title (Extended Mix).flac"),
                    title: format!("A Pretty Long Title {i} (Extended Mix)"),
                    artist: format!("Artist {i}"),
                    album: format!("An Album {i}"),
                    genre: "Progressive House".into(),
                    duration_ms: 384_000,
                    bpm: Some(126),
                    updated_at: 1_755_000_000_000,
                    present: true,
                })
                .collect(),
            playlists: (0..40)
                .map(|i| PlaylistEntry {
                    uid: uid(900_000 + i),
                    name: format!("Playlist set {i}"),
                    kind: "playlist".into(),
                    parent_uid: None,
                    rank: "aZk".into(),
                    updated_at: 1_755_000_000_000,
                })
                .collect(),
            // A track is usually in several playlists, and each membership
            // is its own row in the inventory.
            memberships: (0..tracks)
                .flat_map(|i| {
                    (0..playlists_per_track).map(move |p| Membership {
                        playlist_uid: uid(900_000 + p),
                        track_uid: uid(i),
                        rank: "aZkQm".into(),
                        added_at: 1_755_000_000_000,
                    })
                })
                .collect(),
            tombstones: Vec::new(),
            scopes: Vec::new(),
            device_sync: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(uid: &str, hash: Option<&str>, updated_at: i64) -> TrackEntry {
        TrackEntry {
            uid: uid.into(),
            hash: hash.map(|h| h.into()),
            size: 1000,
            filename: format!("{uid}.flac"),
            title: uid.into(),
            artist: String::new(),
            album: String::new(),
            genre: String::new(),
            duration_ms: 0,
            bpm: None,
            updated_at,
            present: true,
        }
    }

    fn manifest(tracks: Vec<TrackEntry>) -> Manifest {
        Manifest {
            device_uid: "dev".into(),
            tracks,
            playlists: Vec::new(),
            memberships: Vec::new(),
            tombstones: Vec::new(),
            scopes: Vec::new(),
            device_sync: Vec::new(),
        }
    }

    /// Both test manifests share `device_uid` ("dev"), which works for
    /// everything except scope: that needs two distinct identities.
    fn named(uid: &str, m: Manifest) -> Manifest {
        Manifest { device_uid: uid.into(), ..m }
    }

    /// A playlist gets unmarked for the phone and then a track from that
    /// playlist gets changed on the PC: the file can't travel. Not right
    /// after unmarking, and not once the phone already freed the space —
    /// missing it isn't a reason to send it, or "freeing space" wouldn't
    /// actually free anything.
    #[test]
    fn an_unselected_playlist_never_sends_its_files() {
        let plists = vec![
            PlaylistEntry {
                uid: "x".into(),
                name: "Unmarked".into(),
                kind: "playlist".into(),
                parent_uid: None,
                rank: "V".into(),
                updated_at: 0,
            },
            PlaylistEntry {
                uid: "y".into(),
                name: "Marked".into(),
                kind: "playlist".into(),
                parent_uid: None,
                rank: "W".into(),
                updated_at: 0,
            },
        ];
        // The track only lives in the unmarked one.
        let members = vec![Membership {
            playlist_uid: "x".into(),
            track_uid: "t".into(),
            rank: "V".into(),
            added_at: 0,
        }];
        let scopes = vec![
            ScopeEntry {
                device_uid: "celu".into(),
                playlist_uid: "y".into(),
                selected: true,
                updated_at: 10,
            },
            ScopeEntry {
                device_uid: "celu".into(),
                playlist_uid: "x".into(),
                selected: false,
                updated_at: 20,
            },
        ];
        let modes = vec![DeviceSync {
            device_uid: "celu".into(),
            mode: "selected".into(),
            direction: "both".into(),
            updated_at: 10,
        }];
        let with = |uid: &str, t: TrackEntry| {
            let mut m = named(uid, manifest(vec![t]));
            m.playlists = plists.clone();
            m.memberships = members.clone();
            m.scopes = scopes.clone();
            m.device_sync = modes.clone();
            m
        };

        // The PC re-tagged the track: same uid, different bytes, newer.
        let pc = with("pc", track("t", Some("h-new"), 500));
        let celu = with("celu", track("t", Some("h-old"), 100));

        let p = plan(&pc, &celu);
        assert!(p.push_files.is_empty(), "should not go out to an unmarked playlist");
        assert_eq!(p.out_of_scope_out, 1);

        // And the phone reaches the same conclusion without negotiating anything.
        let p = plan(&celu, &pc);
        assert!(p.pull_files.is_empty());
        assert_eq!(p.out_of_scope_in, 1);

        // After "freeing up space" the file isn't there on the other side.
        let mut freed = celu.clone();
        freed.tracks[0].present = false;
        let p = plan(&pc, &freed);
        assert!(p.push_files.is_empty(), "freeing space can't make it come back down");
        assert_eq!(p.out_of_scope_out, 1);
    }

    #[test]
    fn missing_files_are_planned_in_both_directions() {
        let local = manifest(vec![track("a", Some("h-a"), 0)]);
        let remote = manifest(vec![track("b", Some("h-b"), 0)]);
        let p = plan(&local, &remote);
        assert_eq!(p.pull_files.len(), 1);
        assert_eq!(p.pull_files[0].hash, "h-b");
        assert_eq!(p.push_files.len(), 1);
        assert_eq!(p.push_files[0].hash, "h-a");
        assert_eq!(p.bytes_in(), 1000);
    }

    /// The same file imported separately on two devices has different uids
    /// but the same content. Transferring it would burn bandwidth just to
    /// end up with two identical copies.
    #[test]
    fn same_content_under_different_uids_is_not_transferred() {
        let local = manifest(vec![track("here", Some("same-hash"), 0)]);
        let remote = manifest(vec![track("there", Some("same-hash"), 0)]);
        let p = plan(&local, &remote);
        assert!(p.pull_files.is_empty());
        assert!(p.push_files.is_empty());
    }

    /// Without this, every sync would resurrect what was deleted and there'd
    /// be no way to actually remove anything from the library.
    #[test]
    fn deleted_tracks_are_not_pulled_back() {
        let mut local = manifest(vec![]);
        local.tombstones.push(Tombstone {
            entity: "track".into(),
            uid: "deleted".into(),
            deleted_at: 100,
        });
        let remote = manifest(vec![track("deleted", Some("h"), 0)]);
        let p = plan(&local, &remote);
        assert!(p.pull_files.is_empty(), "should not bring back what I deleted");
        // And on the other side there's indeed something to delete.
        assert_eq!(p.deletes_out, 1);
        assert_eq!(p.deletes_in, 0);
    }

    #[test]
    fn newer_metadata_counts_on_the_right_side() {
        let local = manifest(vec![track("x", Some("h"), 100)]);
        let remote = manifest(vec![track("x", Some("h"), 500)]);
        let p = plan(&local, &remote);
        assert_eq!(p.pull_meta, 1);
        assert_eq!(p.push_meta, 0);

        let p = plan(&remote, &local);
        assert_eq!(p.pull_meta, 0);
        assert_eq!(p.push_meta, 1);
    }

    /// The track is already here, with a file, but the backfill hasn't
    /// computed its hash yet. Pulling it "just in case" means transferring
    /// it in full on every sync for as long as that window lasts — the same
    /// track traveling back and forth while present on both sides.
    #[test]
    fn a_track_already_here_but_not_hashed_yet_is_not_downloaded_again() {
        let local = manifest(vec![track("t1", None, 0)]);
        let remote = manifest(vec![track("t1", Some("h1"), 0)]);
        assert!(plan(&local, &remote).pull_files.is_empty());

        // And if we truly don't have it, it does get pulled.
        assert_eq!(plan(&manifest(vec![]), &remote).pull_files.len(), 1);
    }

    /// Same track, different bytes on each side. If each side pulls the
    /// other's, they'd swap it forever — and every round sends a whole file
    /// over the network and archives the previous one. The plan has to pick
    /// a winner **and both ends have to pick the same one**.
    #[test]
    fn the_same_track_with_different_bytes_moves_once_and_in_one_direction() {
        let mut old = track("t1", Some("hash-old"), 100);
        let mut new = track("t1", Some("hash-new"), 500);
        old.filename = "track.mp3".into();
        new.filename = "track.mp3".into();

        // From the side that has the old one: it pulls the new one, sends nothing.
        let p = plan(&manifest(vec![old.clone()]), &manifest(vec![new.clone()]));
        assert_eq!(p.pull_files.len(), 1);
        assert!(p.push_files.is_empty());

        // From the side that has the new one: the conclusion is the same,
        // reversed. If it also pulled here, they'd be swapping it back and forth.
        let p = plan(&manifest(vec![new.clone()]), &manifest(vec![old.clone()]));
        assert!(p.pull_files.is_empty(), "should not pull an older version");
        assert_eq!(p.push_files.len(), 1);

        // On a tie, the hash breaks it, but it still moves in a single
        // direction: never both.
        let a = track("t1", Some("aaa"), 100);
        let b = track("t1", Some("bbb"), 100);
        let one = plan(&manifest(vec![a.clone()]), &manifest(vec![b.clone()]));
        let other = plan(&manifest(vec![b]), &manifest(vec![a]));
        assert_eq!(one.pull_files.len() + other.pull_files.len(), 1);
    }

    #[test]
    fn tracks_without_hash_do_not_participate() {
        let local = manifest(vec![track("a", None, 0)]);
        let remote = manifest(vec![track("b", None, 0)]);
        let p = plan(&local, &remote);
        assert!(p.pull_files.is_empty());
        assert!(p.push_files.is_empty());
        assert_eq!(p.unhashed, 1);
    }

    /// A track evacuated by selective sync still stays in the library as a
    /// row, but its file isn't there: it can't be offered as a source.
    #[test]
    fn absent_files_are_not_offered_as_a_source() {
        let mut remote = manifest(vec![track("a", Some("h-a"), 0)]);
        remote.tracks[0].present = false;
        let p = plan(&manifest(vec![]), &remote);
        assert!(p.pull_files.is_empty());
    }

    #[test]
    fn memberships_are_unioned_unless_explicitly_removed() {
        let mut local = manifest(vec![]);
        let mut remote = manifest(vec![]);
        remote.memberships.push(Membership {
            playlist_uid: "pl".into(),
            track_uid: "tr".into(),
            rank: "V".into(),
            added_at: 0,
        });
        // No tombstone: it should be pulled.
        assert_eq!(plan(&local, &remote).pull_memberships, 1);

        // With an explicit local tombstone: it doesn't come back.
        local.tombstones.push(Tombstone {
            entity: "playlist_track".into(),
            uid: "pl:tr".into(),
            deleted_at: 10,
        });
        let p = plan(&local, &remote);
        assert_eq!(p.pull_memberships, 0);
        assert_eq!(p.deletes_out, 1);
    }

    #[test]
    fn playlists_missing_on_either_side_are_counted() {
        let mut local = manifest(vec![]);
        let mut remote = manifest(vec![]);
        local.playlists.push(PlaylistEntry {
            uid: "here-only".into(),
            name: "Sets".into(),
            kind: "playlist".into(),
            parent_uid: None,
            rank: "V".into(),
            updated_at: 0,
        });
        remote.playlists.push(PlaylistEntry {
            uid: "there-only".into(),
            name: "Warmup".into(),
            kind: "playlist".into(),
            parent_uid: None,
            rank: "V".into(),
            updated_at: 0,
        });
        let p = plan(&local, &remote);
        assert_eq!(p.pull_playlists, 1);
        assert_eq!(p.push_playlists, 1);
    }

    /// A tombstone for something the peer already lacks isn't pending work.
    #[test]
    fn tombstones_for_things_the_peer_already_lacks_are_not_counted() {
        let mut local = manifest(vec![]);
        local.tombstones.push(Tombstone {
            entity: "track".into(),
            uid: "ghost".into(),
            deleted_at: 1,
        });
        let p = plan(&local, &manifest(vec![]));
        assert_eq!(p.deletes_out, 0);
        assert!(p.is_empty());
    }

    /// Selective sync: the phone only pulls files for marked playlists — but
    /// the playlist and the membership travel regardless, so the track
    /// still shows up in its library even without the file.
    #[test]
    fn out_of_scope_files_are_not_pulled_but_the_library_still_travels() {
        let mut remote = named("pc", manifest(vec![track("t1", Some("h1"), 0), track("t2", Some("h2"), 0)]));
        remote.playlists.push(PlaylistEntry {
            uid: "sets".into(),
            name: "Sets".into(),
            kind: "playlist".into(),
            parent_uid: None,
            rank: "V".into(),
            updated_at: 0,
        });
        remote.memberships.push(Membership {
            playlist_uid: "sets".into(),
            track_uid: "t1".into(),
            rank: "V".into(),
            added_at: 0,
        });

        let mut local = named("celu", manifest(vec![]));
        local.device_sync.push(DeviceSync {
            device_uid: "celu".into(),
            mode: "selected".into(),
            direction: "both".into(),
            updated_at: 10,
        });

        // Nothing marked yet: no files, but both playlists still travel.
        let p = plan(&local, &remote);
        assert!(p.pull_files.is_empty());
        assert_eq!(p.out_of_scope_in, 2);
        assert_eq!(p.pull_playlists, 1);
        assert_eq!(p.pull_memberships, 1);

        // Marking "Sets" pulls down only the track inside it.
        local.scopes.push(ScopeEntry {
            device_uid: "celu".into(),
            playlist_uid: "sets".into(),
            selected: true,
            updated_at: 20,
        });
        let p = plan(&local, &remote);
        assert_eq!(p.pull_files.len(), 1);
        assert_eq!(p.pull_files[0].track_uid, "t1");
        assert_eq!(p.out_of_scope_in, 1);
    }

    /// The peer's scope decides what gets sent to it: unmarking a playlist
    /// from the PC has to cut off the send to the phone.
    #[test]
    fn the_peers_scope_decides_what_gets_pushed() {
        let local = named("pc", manifest(vec![track("t1", Some("h1"), 0)]));
        let mut remote = named("celu", manifest(vec![]));
        remote.device_sync.push(DeviceSync {
            device_uid: "celu".into(),
            mode: "selected".into(),
            direction: "both".into(),
            updated_at: 10,
        });
        let p = plan(&local, &remote);
        assert!(p.push_files.is_empty());
        assert_eq!(p.out_of_scope_out, 1);
    }

    #[test]
    fn identical_libraries_produce_an_empty_plan() {
        let a = manifest(vec![track("x", Some("h"), 10)]);
        let b = manifest(vec![track("x", Some("h"), 10)]);
        assert!(plan(&a, &b).is_empty());
    }
}
