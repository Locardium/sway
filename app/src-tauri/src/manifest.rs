//! Inventario de la biblioteca y cálculo del plan de sync (Fase 5.3).
//!
//! Acá NO se escribe nada. Los dos lados se mandan lo que tienen y se calcula
//! qué habría que hacer; recién 5.4 lo ejecuta. Separarlo así es a propósito:
//! el merge es la parte donde un error borra música, y se puede probar entera
//! —y mirar con los ojos, en la UI— antes de que pueda tocar un archivo.
//!
//! `plan()` es una función pura sobre dos manifests. Todas las decisiones
//! difíciles viven ahí y se testean sin red, sin DB y sin dispositivos.

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// Inventario
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackEntry {
    pub uid: String,
    /// `None` mientras el backfill de hashes no llegó a este track. Sin hash
    /// no se puede pedir ni verificar el archivo, así que no participa.
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
    /// El archivo está en este dispositivo (no fue evacuado por sync
    /// selectiva).
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tombstone {
    pub entity: String,
    pub uid: String,
    pub deleted_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub device_uid: String,
    pub tracks: Vec<TrackEntry>,
    pub playlists: Vec<PlaylistEntry>,
    pub memberships: Vec<Membership>,
    pub tombstones: Vec<Tombstone>,
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

/// Lo que pasaría si se sincronizara ahora. Los conteos son lo que se muestra
/// en la UI antes de habilitar nada.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    /// Archivos que tiene el otro y acá faltan.
    pub pull_files: Vec<FileTransfer>,
    /// Archivos que hay acá y al otro le faltan.
    pub push_files: Vec<FileTransfer>,
    /// Tracks cuya metadata es más nueva del otro lado (y al revés).
    pub pull_meta: usize,
    pub push_meta: usize,
    /// Playlists/carpetas que no existen del otro lado (y al revés).
    pub pull_playlists: usize,
    pub push_playlists: usize,
    /// Tracks agregados a playlists que del otro lado no figuran.
    pub pull_memberships: usize,
    pub push_memberships: usize,
    /// Borrados que habría que aplicar acá / allá.
    pub deletes_in: usize,
    pub deletes_out: usize,
    /// Tracks que no participan porque todavía no tienen hash calculado.
    pub unhashed: usize,
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

fn tombstoned(m: &Manifest, entity: &str, uid: &str) -> bool {
    m.tombstones
        .iter()
        .any(|t| t.entity == entity && t.uid == uid)
}

fn pair_key(playlist_uid: &str, track_uid: &str) -> String {
    format!("{playlist_uid}:{track_uid}")
}

/// Calcula qué haría un sync entre `local` y `remote`. No toca nada.
///
/// Sesgos, todos en la misma dirección — ante la duda, conservar:
/// - Un track cuyo `uid` fue borrado acá NO se vuelve a traer. Sin esto, cada
///   sync resucitaría lo borrado y no habría forma de sacar nada de la
///   biblioteca.
/// - Un archivo se considera presente si coincide el **contenido** (hash), no
///   el uid: el mismo MP3 importado por separado en dos dispositivos tiene
///   uids distintos, y transferirlo de nuevo sería tirar ancho de banda para
///   terminar con dos copias iguales.
/// - Las membresías (track dentro de playlist) se unen; sacar sólo se
///   propaga si hay un tombstone explícito.
pub fn plan(local: &Manifest, remote: &Manifest) -> Plan {
    let mut p = Plan::default();

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

    p.unhashed = local.tracks.iter().filter(|t| t.hash.is_none()).count();

    // --- Archivos -----------------------------------------------------------
    for t in remote.tracks.iter().filter(|t| t.present) {
        let Some(hash) = t.hash.as_deref() else { continue };
        if local_hashes.contains(hash) || tombstoned(local, "track", &t.uid) {
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
        if remote_hashes.contains(hash) || tombstoned(remote, "track", &t.uid) {
            continue;
        }
        p.push_files.push(FileTransfer {
            track_uid: t.uid.clone(),
            hash: hash.to_string(),
            filename: t.filename.clone(),
            size: t.size,
        });
    }

    // --- Metadata (LWW por fila; el detalle por campo llega en 5.5) ---------
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
        .filter(|pl| !local_pl.contains(pl.uid.as_str()) && !tombstoned(local, "playlist", &pl.uid))
        .count();
    p.push_playlists = local
        .playlists
        .iter()
        .filter(|pl| !remote_pl.contains(pl.uid.as_str()) && !tombstoned(remote, "playlist", &pl.uid))
        .count();

    // --- Membresías ---------------------------------------------------------
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
            !local_pairs.contains(&k) && !tombstoned(local, "playlist_track", &k)
        })
        .count();
    p.push_memberships = local
        .memberships
        .iter()
        .filter(|m| {
            let k = pair_key(&m.playlist_uid, &m.track_uid);
            !remote_pairs.contains(&k) && !tombstoned(remote, "playlist_track", &k)
        })
        .count();

    // --- Borrados -----------------------------------------------------------
    // Un tombstone sólo cuenta si del otro lado todavía existe eso que borra.
    p.deletes_in = remote
        .tombstones
        .iter()
        .filter(|t| exists_in(local, &t.entity, &t.uid))
        .count();
    p.deletes_out = local
        .tombstones
        .iter()
        .filter(|t| exists_in(remote, &t.entity, &t.uid))
        .count();

    p
}

fn exists_in(m: &Manifest, entity: &str, uid: &str) -> bool {
    match entity {
        "track" => m.tracks.iter().any(|t| t.uid == uid),
        "playlist" => m.playlists.iter().any(|p| p.uid == uid),
        "playlist_track" => m
            .memberships
            .iter()
            .any(|x| pair_key(&x.playlist_uid, &x.track_uid) == uid),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Construcción desde la DB
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

    // `parent_uid` en vez de `parent_id`: los ids INTEGER son locales y no
    // significan nada del otro lado.
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
            "SELECT p.uid, t.uid, pt.rank
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
    })
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
        }
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

    /// El mismo archivo importado por separado en dos dispositivos tiene uids
    /// distintos pero el mismo contenido. Transferirlo sería gastar ancho de
    /// banda para terminar con dos copias idénticas.
    #[test]
    fn same_content_under_different_uids_is_not_transferred() {
        let local = manifest(vec![track("aca", Some("mismo-hash"), 0)]);
        let remote = manifest(vec![track("alla", Some("mismo-hash"), 0)]);
        let p = plan(&local, &remote);
        assert!(p.pull_files.is_empty());
        assert!(p.push_files.is_empty());
    }

    /// Sin esto, cada sync resucitaría lo borrado y no habría manera de sacar
    /// nada de la biblioteca.
    #[test]
    fn deleted_tracks_are_not_pulled_back() {
        let mut local = manifest(vec![]);
        local.tombstones.push(Tombstone {
            entity: "track".into(),
            uid: "borrado".into(),
            deleted_at: 100,
        });
        let remote = manifest(vec![track("borrado", Some("h"), 0)]);
        let p = plan(&local, &remote);
        assert!(p.pull_files.is_empty(), "no debe volver lo que borré");
        // Y del otro lado sí hay algo para borrar.
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

    #[test]
    fn tracks_without_hash_do_not_participate() {
        let local = manifest(vec![track("a", None, 0)]);
        let remote = manifest(vec![track("b", None, 0)]);
        let p = plan(&local, &remote);
        assert!(p.pull_files.is_empty());
        assert!(p.push_files.is_empty());
        assert_eq!(p.unhashed, 1);
    }

    /// Un track evacuado por sync selectiva sigue en la biblioteca como fila,
    /// pero su archivo no está: no puede ofrecerse como fuente.
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
        });
        // Sin tombstone: hay que traerla.
        assert_eq!(plan(&local, &remote).pull_memberships, 1);

        // Con tombstone local explícito: no vuelve.
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
            uid: "solo-aca".into(),
            name: "Sets".into(),
            kind: "playlist".into(),
            parent_uid: None,
            rank: "V".into(),
            updated_at: 0,
        });
        remote.playlists.push(PlaylistEntry {
            uid: "solo-alla".into(),
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

    /// Un tombstone de algo que el otro ya no tiene no es trabajo pendiente.
    #[test]
    fn tombstones_for_things_the_peer_already_lacks_are_not_counted() {
        let mut local = manifest(vec![]);
        local.tombstones.push(Tombstone {
            entity: "track".into(),
            uid: "fantasma".into(),
            deleted_at: 1,
        });
        let p = plan(&local, &manifest(vec![]));
        assert_eq!(p.deletes_out, 0);
        assert!(p.is_empty());
    }

    #[test]
    fn identical_libraries_produce_an_empty_plan() {
        let a = manifest(vec![track("x", Some("h"), 10)]);
        let b = manifest(vec![track("x", Some("h"), 10)]);
        assert!(plan(&a, &b).is_empty());
    }
}
