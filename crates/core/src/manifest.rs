//! Inventario de la biblioteca y cálculo del plan de sync (Fase 5.3).
//!
//! Acá NO se escribe nada. Los dos lados se mandan lo que tienen y se calcula
//! qué habría que hacer; recién 5.4 lo ejecuta. Separarlo así es a propósito:
//! el merge es la parte donde un error borra música, y se puede probar entera
//! —y mirar con los ojos, en la UI— antes de que pueda tocar un archivo.
//!
//! `plan()` es una función pura sobre dos manifests. Todas las decisiones
//! difíciles viven ahí y se testean sin red, sin DB y sin dispositivos.

use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};

// ---------------------------------------------------------------------------
// Compresión del inventario
// ---------------------------------------------------------------------------

/// Tope de lo que se acepta descomprimir.
///
/// Un `Vec` comprimido chiquito puede expandirse a gigabytes si lo armó
/// alguien con ganas de que el otro lado se quede sin memoria. El límite es el
/// mismo que ya tenía el canal para un mensaje entero (ver `wire.rs`): un
/// inventario más grande que esto es un problema aunque venga sin comprimir.
const MAX_INFLATED: u64 = 64 * 1024 * 1024;

/// Comprime el inventario.
///
/// El inventario es JSON y es de lo más comprimible que hay: las mismas claves
/// repetidas en cada fila, uuids y hashes en hexadecimal. Y no es un detalle
/// de eficiencia — es lo que viaja **entero** en cada comparación, haya
/// cambiado algo o no, y del celular sale por el plan de datos.
pub fn squeeze(json: &[u8]) -> Result<Vec<u8>> {
    // Compresión rápida y no máxima: la diferencia entre los dos extremos son
    // unos pocos puntos porcentuales sobre algo que ya baja diez veces, y el
    // lado que sirve puede ser una Raspberry.
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    enc.write_all(json)?;
    Ok(enc.finish()?)
}

/// Lo devuelve a JSON.
pub fn expand(gz: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(gz)
        .take(MAX_INFLATED)
        .read_to_end(&mut out)?;
    Ok(out)
}

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
    /// Cuando entro el track a la playlist. Se compara contra el
    /// `deleted_at` del tombstone del mismo par: gana el mas reciente.
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

/// Una playlist marcada (o desmarcada) para un dispositivo. Viaja porque el
/// scope se edita desde cualquier lado — ver `scope.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopeEntry {
    pub device_uid: String,
    pub playlist_uid: String,
    pub selected: bool,
    pub updated_at: i64,
}

/// Lo que hace un dispositivo: qué dirección sincroniza y si se lleva toda la
/// biblioteca o sólo lo marcado. Es una propiedad del dispositivo, no del
/// vínculo — entre dos, A → B pasa sólo si A manda y B recibe, y las dos filas
/// las tienen los dos lados.
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
    /// Scope de todos los dispositivos conocidos (Fase 5.7). `default` para
    /// poder leer un manifest de una versión anterior sin romper.
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
    /// Archivos que NO se traen / NO se mandan porque quedaron fuera del scope
    /// selectivo del dispositivo que los recibiría (Fase 5.7). No son trabajo
    /// pendiente: son la sync selectiva funcionando. Se muestran igual, porque
    /// "faltan 300 archivos y el plan dice cero" necesita explicación.
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

/// El mismo track (mismo uid) con bytes distintos en cada dispositivo: otra
/// codificación, otro recorte, un tagger que reescribió el archivo.
///
/// Hay que quedarse con uno solo, y **los dos lados tienen que elegir el
/// mismo**. Si cada uno se trae el del otro, se lo intercambian en cada sync
/// para siempre: A termina con el de B, B con el de A, y en la vuelta
/// siguiente al revés. Por eso la regla no puede ser "traé lo que te falta"
/// sino una comparación que dé idéntica en las dos puntas.
///
/// Gana el más nuevo; a igual fecha desempata el hash. El desempate es
/// arbitrario, pero es lo único que importa: que sea el mismo de los dos lados.
fn wins(a: &TrackEntry, b: &TrackEntry) -> bool {
    (a.updated_at, a.hash.as_deref()) > (b.updated_at, b.hash.as_deref())
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
/// - El scope selectivo filtra **sólo archivos**: las playlists, el orden y la
///   metadata viajan enteros igual. Un track fuera de scope se sigue viendo en
///   la biblioteca del otro dispositivo, sin archivo.
pub fn plan(local: &Manifest, remote: &Manifest) -> Plan {
    let mut p = Plan::default();

    // Scope resuelto con las filas más nuevas de los dos lados: durante la
    // ventana en que un cambio todavía no viajó, los manifests difieren y hay
    // que decidir con la más reciente, no con la propia.
    let all_entries = crate::scope::merge_entries(&local.scopes, &remote.scopes);
    let all_modes = crate::scope::merge_device_sync(&local.device_sync, &remote.device_sync);
    // La jerarquía y las membresías se resuelven sobre la unión: una playlist
    // que sólo existe de un lado igual define qué entra.
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

    // Los tombstones se consultan una vez por track, por playlist y por
    // membresía. Recorrer la lista en cada consulta hace el sync cuadrático:
    // con 20 mil tracks y unos cuantos borrados son cientos de millones de
    // comparaciones de strings, dos veces por corrida.
    let local_tombs: HashSet<(&str, &str)> = tombstone_keys(local);
    let remote_tombs: HashSet<(&str, &str)> = tombstone_keys(remote);
    let tombstoned = |set: &HashSet<(&str, &str)>, entity: &str, uid: &str| {
        set.contains(&(entity, uid))
    };

    p.unhashed = local.tracks.iter().filter(|t| t.hash.is_none()).count();

    // --- Archivos -----------------------------------------------------------
    for t in remote.tracks.iter().filter(|t| t.present) {
        let Some(hash) = t.hash.as_deref() else { continue };
        if local_hashes.contains(hash) || tombstoned(&local_tombs, "track", &t.uid) {
            continue;
        }
        if let Some(l) = local_by_uid.get(t.uid.as_str()).filter(|l| l.present) {
            // Ese track ya está acá pero todavía sin hash calculado: no se
            // puede saber si es el mismo contenido. Bajarlo "por las dudas" es
            // transferirlo entero en CADA sync mientras el backfill no llega —
            // que es exactamente lo que pasaba: el mismo tema viajando una y
            // otra vez estando en los dos lados. Se espera al hash.
            if l.hash.is_none() {
                continue;
            }
            // Está acá con otros bytes: sólo se trae si el de allá gana el
            // desempate (ver `wins`), o los dos se intercambian el archivo
            // para siempre.
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
        // Espejo de lo de arriba, visto desde el otro lado: no se le manda algo
        // que allá ya está bajo el mismo uid, salvo que lo nuestro gane el
        // desempate. Los dos lados hacen la misma cuenta y llegan al mismo
        // ganador, así que el archivo viaja una vez y en una sola dirección.
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
        .filter(|pl| !local_pl.contains(pl.uid.as_str()) && !tombstoned(&local_tombs, "playlist", &pl.uid))
        .count();
    p.push_playlists = local
        .playlists
        .iter()
        .filter(|pl| !remote_pl.contains(pl.uid.as_str()) && !tombstoned(&remote_tombs, "playlist", &pl.uid))
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

    // --- Borrados -----------------------------------------------------------
    // Un tombstone sólo cuenta si del otro lado todavía existe eso que borra.
    // Los conjuntos ya están armados arriba: preguntar recorriendo la lista
    // por cada tombstone volvía esto cuadrático.
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
mod compresion {
    use super::*;

    #[test]
    fn lo_que_se_comprime_vuelve_igual() {
        let json = br#"{"tracks":[{"uid":"a","title":"Un tema"}],"playlists":[]}"#;
        let gz = squeeze(json).unwrap();
        assert_eq!(expand(&gz).unwrap(), json);
    }

    /// Un inventario vacío también tiene que sobrevivir: es lo que manda un
    /// dispositivo recién instalado, y es justo el caso donde equivocarse
    /// borraría la biblioteca del otro lado.
    #[test]
    fn un_inventario_vacio_sobrevive() {
        let json = b"{}";
        assert_eq!(expand(&squeeze(json).unwrap()).unwrap(), json);
    }

    /// Basura comprimida no puede colgar ni reventar al que la recibe: tiene
    /// que dar error y nada más.
    #[test]
    fn algo_que_no_es_gzip_da_error_y_no_panic() {
        assert!(expand(b"esto no es un gzip ni por casualidad").is_err());
    }
}

#[cfg(test)]
mod peso {
    use super::*;

    /// Cuánto pesa el inventario que viaja en CADA sync, haya cambiado algo o
    /// no. No es una aserción, es una medición: correr con
    /// `cargo test -p sway-core -- --ignored --nocapture peso`.
    ///
    /// Importa porque el inventario no depende de lo que cambió sino de lo que
    /// hay: una biblioteca quieta cuesta exactamente lo mismo que una que se
    /// movió entera. Es lo que hace que la pasada periódica escale con el
    /// tamaño de la biblioteca en vez de con la cantidad de novedades.
    #[test]
    #[ignore]
    fn cuanto_pesa_el_inventario() {
        for (tracks, por_tema) in [(100usize, 3usize), (1_000, 3), (5_000, 3)] {
            let m = sintetico(tracks, por_tema);
            let json = serde_json::to_vec(&m).unwrap();
            let gz = crate::manifest::squeeze(&json).unwrap();
            println!(
                "{tracks} temas, {} membresías -> {:.2} MB en crudo, {:.2} MB comprimido ({:.1}x)",
                m.memberships.len(),
                json.len() as f64 / (1024.0 * 1024.0),
                gz.len() as f64 / (1024.0 * 1024.0),
                json.len() as f64 / gz.len() as f64,
            );
        }
    }

    /// Una biblioteca de mentira con medidas realistas: uid de UUID, hash de
    /// blake3 en hexa, y títulos y nombres de archivo de largo corriente.
    fn sintetico(tracks: usize, playlists_por_tema: usize) -> Manifest {
        let uid = |n: usize| format!("{n:08x}-1111-4222-8333-444455556666");
        let hash = |n: usize| format!("{:064x}", n);
        Manifest {
            device_uid: uid(0),
            tracks: (0..tracks)
                .map(|i| TrackEntry {
                    uid: uid(i),
                    hash: Some(hash(i)),
                    size: 9_325_265,
                    filename: format!("Artista {i} - Un Titulo Bastante Largo (Extended Mix).flac"),
                    title: format!("Un Titulo Bastante Largo {i} (Extended Mix)"),
                    artist: format!("Artista {i}"),
                    album: format!("Un Album {i}"),
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
                    name: format!("Set de la playlist {i}"),
                    kind: "playlist".into(),
                    parent_uid: None,
                    rank: "aZk".into(),
                    updated_at: 1_755_000_000_000,
                })
                .collect(),
            // Un tema suele estar en varias playlists, y cada pertenencia es
            // una fila propia en el inventario.
            memberships: (0..tracks)
                .flat_map(|i| {
                    (0..playlists_por_tema).map(move |p| Membership {
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

    /// Los dos manifests de prueba comparten `device_uid` ("dev"), que sirve
    /// para todo menos para el scope: ahí hacen falta dos identidades.
    fn named(uid: &str, m: Manifest) -> Manifest {
        Manifest { device_uid: uid.into(), ..m }
    }

    /// Se desmarca una playlist para el celular y después se cambia un tema de
    /// esa playlist en la PC: el archivo no puede viajar. Ni recién desmarcada,
    /// ni cuando el celular ya liberó el espacio — que le falte no es motivo
    /// para mandárselo, o "liberar" no liberaría nada.
    #[test]
    fn an_unselected_playlist_never_sends_its_files() {
        let plists = vec![
            PlaylistEntry {
                uid: "x".into(),
                name: "Desmarcada".into(),
                kind: "playlist".into(),
                parent_uid: None,
                rank: "V".into(),
                updated_at: 0,
            },
            PlaylistEntry {
                uid: "y".into(),
                name: "Marcada".into(),
                kind: "playlist".into(),
                parent_uid: None,
                rank: "W".into(),
                updated_at: 0,
            },
        ];
        // El tema vive sólo en la desmarcada.
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

        // La PC retagueó el tema: mismo uid, otros bytes, más nuevo.
        let pc = with("pc", track("t", Some("h-nuevo"), 500));
        let celu = with("celu", track("t", Some("h-viejo"), 100));

        let p = plan(&pc, &celu);
        assert!(p.push_files.is_empty(), "no sale hacia una playlist desmarcada");
        assert_eq!(p.out_of_scope_out, 1);

        // Y el celu llega a la misma conclusión sin negociar nada.
        let p = plan(&celu, &pc);
        assert!(p.pull_files.is_empty());
        assert_eq!(p.out_of_scope_in, 1);

        // Después de "liberar espacio" el archivo no está del otro lado.
        let mut liberado = celu.clone();
        liberado.tracks[0].present = false;
        let p = plan(&pc, &liberado);
        assert!(p.push_files.is_empty(), "liberar no puede hacer que vuelva a bajar");
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

    /// El track ya está acá, con archivo, pero el backfill todavía no le
    /// calculó el hash. Bajarlo "por si acaso" es transferirlo entero en cada
    /// sync mientras dure esa ventana — el mismo tema viajando una y otra vez
    /// estando en los dos lados.
    #[test]
    fn a_track_already_here_but_not_hashed_yet_is_not_downloaded_again() {
        let local = manifest(vec![track("t1", None, 0)]);
        let remote = manifest(vec![track("t1", Some("h1"), 0)]);
        assert!(plan(&local, &remote).pull_files.is_empty());

        // Y si de verdad no lo tenemos, sí se baja.
        assert_eq!(plan(&manifest(vec![]), &remote).pull_files.len(), 1);
    }

    /// Mismo track, bytes distintos en cada lado. Si cada uno se trae el del
    /// otro, se lo intercambian para siempre —y cada vuelta manda un archivo
    /// entero por la red y archiva el anterior. El plan tiene que elegir un
    /// ganador **y que las dos puntas elijan el mismo**.
    #[test]
    fn the_same_track_with_different_bytes_moves_once_and_in_one_direction() {
        let mut viejo = track("t1", Some("hash-viejo"), 100);
        let mut nuevo = track("t1", Some("hash-nuevo"), 500);
        viejo.filename = "tema.mp3".into();
        nuevo.filename = "tema.mp3".into();

        // Desde el lado que tiene el viejo: se trae el nuevo, no manda nada.
        let p = plan(&manifest(vec![viejo.clone()]), &manifest(vec![nuevo.clone()]));
        assert_eq!(p.pull_files.len(), 1);
        assert!(p.push_files.is_empty());

        // Desde el lado que tiene el nuevo: la conclusión es la misma, al
        // revés. Si acá también trajera, se lo estarían intercambiando.
        let p = plan(&manifest(vec![nuevo.clone()]), &manifest(vec![viejo.clone()]));
        assert!(p.pull_files.is_empty(), "no se trae una versión más vieja");
        assert_eq!(p.push_files.len(), 1);

        // A igual fecha desempata el hash, pero sigue moviéndose en una sola
        // dirección: nunca los dos.
        let a = track("t1", Some("aaa"), 100);
        let b = track("t1", Some("bbb"), 100);
        let uno = plan(&manifest(vec![a.clone()]), &manifest(vec![b.clone()]));
        let otro = plan(&manifest(vec![b]), &manifest(vec![a]));
        assert_eq!(uno.pull_files.len() + otro.pull_files.len(), 1);
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
            added_at: 0,
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

    /// Sync selectiva: el celular sólo se baja los archivos de las playlists
    /// marcadas — pero la playlist y la membresía viajan igual, así que el
    /// track se ve en su biblioteca aunque el archivo no esté.
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

        // Nada marcado todavía: ningún archivo, pero las dos playlists sí.
        let p = plan(&local, &remote);
        assert!(p.pull_files.is_empty());
        assert_eq!(p.out_of_scope_in, 2);
        assert_eq!(p.pull_playlists, 1);
        assert_eq!(p.pull_memberships, 1);

        // Marcando "Sets" baja sólo el track que está adentro.
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

    /// El scope del otro lado decide qué le mando: desmarcar una playlist
    /// desde la PC tiene que cortar el envío al celular.
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
