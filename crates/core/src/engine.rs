//! El motor de sync, sin Tauri adentro (Fase 5.8).
//!
//! Hasta acá la sincronización vivía pegada a `AppHandle`: para leer la base
//! había que pedirle el estado a la app, y para avisar de un archivo recibido
//! había que emitir un evento de ventana. Eso alcanzaba para que anduviera,
//! pero dejaba el requisito duro de toda la Fase 5 —**nunca perder música**—
//! apoyado en probar a mano con dos dispositivos reales.
//!
//! Todo lo que el motor necesita del dispositivo donde corre entra ahora por
//! `Host`: la base, la carpeta gestionada, y los avisos a la UI. La app lo
//! implementa sobre `AppHandle` (ver `lib.rs`); la suite de integridad lo
//! implementa sobre un directorio temporal, y así puede levantar **dos motores
//! en el mismo proceso** y hacerlos sincronizar de verdad sobre loopback —
//! archivos, cortes de red, conflictos y borrados incluidos.
//!
//! Lo que NO entra acá: pairing, descubrimiento, y todo lo que necesita que
//! haya una persona mirando una pantalla. Eso sigue en `pairing.rs`.

use crate::wire::{Msg, Session};
use anyhow::{anyhow, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// El dispositivo donde corre el motor
// ---------------------------------------------------------------------------

/// Avance de un archivo. La app lo convierte en un evento de ventana; la suite
/// de integridad lo usa para cortar la red justo a la mitad de una
/// transferencia, que es la única forma honesta de probar la reanudación.
pub struct Progress<'a> {
    pub peer_uid: &'a str,
    /// Índice del archivo actual y total de archivos de esta corrida.
    pub index: usize,
    pub total_files: usize,
    pub filename: &'a str,
    /// Bytes del archivo actual.
    pub done: u64,
    pub total: u64,
    pub sending: bool,
}

/// Lo que el motor necesita del dispositivo donde corre.
///
/// Las dos formas de tocar la base son closures y no un guard devuelto a
/// propósito: así el que implementa decide cómo toma el lock (y cuándo lo
/// suelta) sin que el motor pueda quedárselo cruzando una operación larga —
/// **nunca sostener el lock de SQLite mientras se hashea o se hace I/O**, que
/// es la regla que ya mordió dos veces en este proyecto.
pub trait Host {
    fn with_db<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T>;

    /// Conexión de sólo lectura, para lo que casi siempre da cero y no vale
    /// encolar detrás de un sync en curso. Por defecto es la misma.
    fn with_db_read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.with_db(f)
    }

    /// Carpeta gestionada: acá viven los archivos, la papelera y los parciales.
    fn music_dir(&self) -> PathBuf;

    /// "Este archivo lo escribí yo": el watcher de la carpeta lo saltea en vez
    /// de auto-importarlo con un uid nuevo. Sin watcher no hace falta nada.
    fn expect_path(&self, _dest: &Path) {}

    fn progress(&self, _p: &Progress) {}

    /// La biblioteca cambió. `force` distingue el fin de una corrida (que
    /// siempre avisa) del goteo de archivo por archivo (que se limita).
    fn library_changed(&self, _force: bool) {}
}

// ---------------------------------------------------------------------------
// Resultado
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct SyncResult {
    pub received: usize,
    pub sent: usize,
    pub failed: usize,
    pub bytes: u64,
    pub organized: usize,
}

/// El otro lado cerró sin decir nada: un sondeo, una app que se fue, un
/// celular que se durmió o se cambió de red.
///
/// `BrokenPipe` entra acá: es lo que da escribir en un socket que el otro ya
/// cerró, o sea el final normal de una sesión que se corta. Sin esto salía
/// como error en la cara del usuario ("broken pipe"), que no significa nada
/// para quien lo lee y encima suena a que se rompió algo.
pub fn is_disconnect(e: &anyhow::Error) -> bool {
    use std::io::ErrorKind::*;
    e.downcast_ref::<std::io::Error>()
        .map(|io| {
            matches!(
                io.kind(),
                UnexpectedEof | ConnectionReset | ConnectionAborted | BrokenPipe | NotConnected
            )
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Lado que atiende
// ---------------------------------------------------------------------------

/// Lo que pasó mientras se atendía a alguien.
///
/// El que atiende no decide nada —responde pedidos— así que sin esto su log es
/// una tira de lineas sueltas sin totales, y no hay forma de leer de un
/// vistazo si una corrida movió algo. Del lado del server, que no tiene
/// pantalla, es el único resumen que existe.
#[derive(Debug, Default)]
pub struct ServeStats {
    /// Archivos que nos empujaron.
    pub received: usize,
    /// Archivos que nos pidieron y mandamos.
    pub sent: usize,
    pub applied: crate::merge::Applied,
}

impl ServeStats {
    pub fn moved_something(&self) -> bool {
        self.received > 0 || self.sent > 0 || self.applied.total() > 0
    }
}

/// Después de presentarse, la sesión queda abierta atendiendo pedidos hasta
/// que el otro corta.
pub fn serve_requests<H: Host>(
    host: &H,
    sess: &mut Session,
    peer_uid: &str,
) -> Result<ServeStats> {
    let mut stats = ServeStats::default();
    loop {
        let msg = match sess.recv() {
            Ok(m) => m,
            // Cortó: fin normal de la sesión, no un error.
            Err(e) if is_disconnect(&e) => return Ok(stats),
            Err(e) => return Err(e),
        };
        match msg {
            Msg::ManifestReq => {
                let manifest = host.with_db(|conn| Ok(crate::manifest::build(conn)?))?;
                sess.send(&Msg::ManifestData {
                    manifest: Box::new(manifest),
                })?;
            }
            // Alguien pide un archivo nuestro.
            Msg::BlobReq { hash, offset } => {
                let path = host.with_db(|conn| Ok(crate::transfer::path_for_hash(conn, &hash)))?;
                match path {
                    Some(p) => {
                        crate::transfer::send_file(sess, &p, offset, &hash)?;
                        stats.sent += 1;
                    }
                    None => sess.send(&Msg::BlobError {
                        reason: format!("I do not have file {hash}"),
                    })?,
                }
            }
            // Nos empujan un archivo.
            Msg::BlobPush {
                track_uid,
                hash,
                filename,
                title,
                artist,
                album,
                genre,
                duration_ms,
                bpm,
                updated_at,
                ..
            } => {
                let music_dir = host.music_dir();
                // Empuje: el otro manda desde cero, no continúa nada nuestro.
                let got = crate::transfer::receive_file(
                    sess,
                    &music_dir,
                    &hash,
                    &filename,
                    false,
                    &mut |_, _| {},
                    &mut |dest| host.expect_path(dest),
                )?;
                host.with_db(|conn| {
                    // Nos lo acaba de mandar: obviamente lo tiene. Es lo que
                    // después permite liberar este archivo sin arriesgar nada.
                    let _ = crate::scope::note_replicas(conn, peer_uid, &[hash.clone()]);
                    // Un fallo dando de alta ESTE archivo no puede cortar la
                    // sesión: el `?` hacía que un solo track problemático
                    // volteara el sync entero, que se reintentaba solo y volvía
                    // a voltearse. Se registra y sigue con el próximo.
                    if let Err(e) = crate::transfer::insert_received(
                        conn,
                        &got.path,
                        &track_uid,
                        &hash,
                        got.bytes,
                        &title,
                        &artist,
                        &album,
                        &genre,
                        duration_ms,
                        bpm,
                        updated_at,
                    ) {
                        log::warn!("[sync] could not register {filename}: {e}");
                    }
                    Ok(())
                })?;
                log::info!("[sync] received {filename} ({} bytes)", got.bytes);
                stats.received += 1;
                // Sin forzar: en una tanda de archivos, una recarga completa
                // de la biblioteca por archivo deja la UI inservible.
                host.library_changed(false);
            }
            // Cambios de organización que nos manda el otro lado.
            Msg::MetaPush { changes } => {
                let music_dir = host.music_dir();
                let applied =
                    host.with_db(|conn| Ok(crate::merge::apply(conn, &changes, &music_dir)?))?;
                if applied.total() > 0 {
                    // Las cinco cifras, no tres. `total()` cuenta también el
                    // scope y los borrados, así que cambiar una dirección o
                    // marcar una playlist entraba acá e imprimía tres ceros:
                    // el log decía "no pasó nada" justo cuando había pasado.
                    log::info!(
                        "[sync] applied {} tracks, {} playlists, {} memberships, {} deletions, {} scope rows",
                        applied.tracks,
                        applied.playlists,
                        applied.memberships,
                        applied.deleted,
                        applied.scope
                    );
                    host.library_changed(true);
                }
                // El scope de este dispositivo lo pudo haber cambiado el otro:
                // si volvió a marcar una playlist, lo liberado se recupera de
                // la papelera antes de que nadie lo pida por la red.
                if applied.scope > 0 {
                    restore(host);
                }
                stats.applied.tracks += applied.tracks;
                stats.applied.playlists += applied.playlists;
                stats.applied.memberships += applied.memberships;
                stats.applied.deleted += applied.deleted;
                stats.applied.scope += applied.scope;
                sess.send(&Msg::MetaAck { applied })?;
            }
            Msg::Bye => return Ok(stats),
            other => return Err(anyhow!("unexpected request: {other:?}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Lado que sincroniza
// ---------------------------------------------------------------------------

/// Pide el inventario del otro lado y calcula el plan. Devuelve también el
/// manifest remoto: la transferencia necesita la metadata de cada track para
/// dar de alta lo que reciba con el uid del otro dispositivo.
/// Devuelve además la dirección efectiva: qué se puede traer y qué se puede
/// mandar, resuelto con lo que dice CADA dispositivo de sí mismo (`takes`,
/// `gives`). No se negocia por la red — las dos filas viajan replicadas en el
/// manifest, así que los dos lados llegan solos a la misma conclusión.
pub fn fetch_plan<H: Host>(
    host: &H,
    sess: &mut Session,
) -> Result<(crate::manifest::Plan, crate::manifest::Manifest, (bool, bool))> {
    sess.send(&Msg::ManifestReq)?;
    let remote = match sess.recv()? {
        Msg::ManifestData { manifest } => *manifest,
        other => return Err(anyhow!("expected the manifest, got {other:?}")),
    };
    let local = host.with_db(|conn| {
        // Quién tiene qué archivo. Es lo único que después permite liberar
        // espacio sin arriesgar la última copia (ver `scope::evictable`).
        let hashes: Vec<String> = remote
            .tracks
            .iter()
            .filter(|t| t.present)
            .filter_map(|t| t.hash.clone())
            .collect();
        crate::scope::note_replicas(conn, &remote.device_uid, &hashes)?;
        Ok(crate::manifest::build(conn)?)
    })?;
    let dir = crate::scope::link_from(
        &local.device_uid,
        &remote.device_uid,
        &local.device_sync,
        &remote.device_sync,
    );
    Ok((crate::manifest::plan(&local, &remote), remote, dir))
}

/// Recorta el plan a lo que la dirección de los dos dispositivos permite. El
/// plan puro dice qué falta; esto dice qué de eso se va a hacer de verdad, que
/// es lo que hay que mostrar antes de apretar Sync.
///
/// Archivos y organización van juntos: si un dispositivo no manda, no manda
/// nada — mover playlists sin los archivos, o al revés, deja las dos
/// bibliotecas describiendo cosas distintas.
pub fn restrict(plan: &mut crate::manifest::Plan, (takes, gives): (bool, bool)) {
    if !takes {
        plan.pull_files.clear();
        plan.pull_meta = 0;
        plan.pull_playlists = 0;
        plan.pull_memberships = 0;
        plan.deletes_in = 0;
    }
    if !gives {
        plan.push_files.clear();
        plan.push_meta = 0;
        plan.push_playlists = 0;
        plan.push_memberships = 0;
        plan.deletes_out = 0;
    }
}

/// Una corrida completa sobre una sesión ya abierta y presentada.
pub fn sync<H: Host>(host: &H, sess: &mut Session, peer_uid: &str) -> Result<SyncResult> {
    // Un sync que tarda es tres cosas distintas —inventario, archivos,
    // organización— y sin medirlas por separado no hay forma de saber cuál es.
    let started = std::time::Instant::now();
    // Antes de planificar: lo que volvió al scope y sigue en la papelera se
    // recupera de acá, no se pide por la red. Si no, re-marcar una playlist
    // recién liberada bajaba de nuevo gigabytes que estaban a un rename de
    // distancia.
    restore(host);
    let (mut plan, remote, dir) = fetch_plan(host, sess)?;
    // La dirección se resuelve acá, no en `plan()`: el plan describe qué falta
    // entre las dos bibliotecas, la dirección describe qué se hace con eso.
    restrict(&mut plan, dir);

    // El cambio de scope pudo haberlo hecho el OTRO dispositivo, y recién
    // aparece ahora, en su manifest. Aplicarlo antes de transferir —y rescatar
    // de la papelera lo que vuelve a entrar— es lo que evita bajar de nuevo
    // por la red gigabytes que están a un `rename` de distancia. Si no, se
    // aplicaría después del bucle de archivos, o sea tarde.
    if !plan.pull_files.is_empty() {
        apply_remote_scope(host, &remote)?;
        // Sólo si algo volvió de verdad: rearmar el manifest local es recorrer
        // la biblioteca entera, y hacerlo en cada sync "por las dudas" es caro
        // al pedo — sobre todo en el celular.
        if restore(host) > 0 {
            let local = host.with_db(|conn| Ok(crate::manifest::build(conn)?))?;
            plan = crate::manifest::plan(&local, &remote);
            restrict(&mut plan, dir);
        }
    }
    let music_dir = host.music_dir();
    let after_plan = started.elapsed();

    let total_files = plan.pull_files.len() + plan.push_files.len();
    let (mut received, mut sent, mut failed, mut bytes) = (0usize, 0usize, 0usize, 0u64);
    let mut index = 0usize;

    // --- Traer lo que falta acá ------------------------------------------
    for f in &plan.pull_files {
        index += 1;
        let entry = remote.tracks.iter().find(|t| t.uid == f.track_uid);
        let Some(entry) = entry else { continue };
        let at = index;
        let mut progress = |done: u64, total: u64| {
            host.progress(&Progress {
                peer_uid,
                index: at,
                total_files,
                filename: &f.filename,
                done,
                total,
                sending: false,
            });
        };
        let got = crate::transfer::pull_file(
            sess,
            &music_dir,
            &f.hash,
            &f.filename,
            &mut progress,
            &mut |dest| host.expect_path(dest),
        );
        match got {
            Ok(got) => {
                host.with_db(|conn| {
                    crate::transfer::insert_received(
                        conn,
                        &got.path,
                        &entry.uid,
                        &f.hash,
                        got.bytes,
                        &entry.title,
                        &entry.artist,
                        &entry.album,
                        &entry.genre,
                        entry.duration_ms,
                        entry.bpm,
                        entry.updated_at,
                    )?;
                    Ok(())
                })?;
                bytes += got.bytes;
                received += 1;
            }
            Err(e) => {
                // Un corte de red no es "este archivo falló": la sesión ya no
                // sirve para nada, y seguir el bucle contra un socket muerto
                // sólo suma errores. Se corta y se reintenta después — el
                // `.part` en disco es lo que hace que eso no cueste nada.
                if is_disconnect(&e) {
                    return Err(e);
                }
                log::warn!("[sync] could not fetch {}: {e}", f.filename);
                failed += 1;
            }
        }
    }

    // --- Mandar lo que falta allá ----------------------------------------
    for f in &plan.push_files {
        index += 1;
        let local = host.with_db(|conn| Ok(local_track(conn, &f.track_uid)))?;
        let Some((path, entry)) = local else {
            failed += 1;
            continue;
        };
        host.progress(&Progress {
            peer_uid,
            index,
            total_files,
            filename: &f.filename,
            done: 0,
            total: f.size as u64,
            sending: true,
        });
        let push = sess.send(&Msg::BlobPush {
            track_uid: entry.uid.clone(),
            hash: f.hash.clone(),
            filename: f.filename.clone(),
            size: f.size as u64,
            title: entry.title.clone(),
            artist: entry.artist.clone(),
            album: entry.album.clone(),
            genre: entry.genre.clone(),
            duration_ms: entry.duration_ms,
            bpm: entry.bpm,
            updated_at: entry.updated_at,
        });
        // Un archivo que no se puede leer no corta la corrida entera.
        match push.and_then(|_| crate::transfer::send_file(sess, &path, 0, &f.hash)) {
            Ok(()) => {
                bytes += f.size as u64;
                sent += 1;
                // Ahora el archivo vive también allá: cuenta como respaldo
                // para poder liberar espacio acá.
                let _ = host.with_db(|conn| {
                    let _ = crate::scope::note_replicas(conn, peer_uid, &[f.hash.clone()]);
                    Ok(())
                });
            }
            Err(e) => {
                if is_disconnect(&e) {
                    return Err(e);
                }
                log::warn!("[sync] could not send {}: {e}", f.filename);
                failed += 1;
            }
        }
    }

    // --- Organización (Fase 5.5) ------------------------------------------
    //
    // Después de los archivos a propósito: una membresía de un track que
    // todavía no llegó se ignora, así que primero conviene que exista.
    //
    // El manifest local se reconstruye acá y no se reusa el de `fetch_plan`:
    // las filas que acaban de entrar por transferencia tienen que viajar en
    // este mismo sync, no en el siguiente.
    let local = host.with_db(|conn| Ok(crate::manifest::build(conn)?))?;
    // Con la dirección de metadata cortada el intercambio igual ocurre, vacío:
    // el ida y vuelta MetaPush/MetaAck es la forma del protocolo, y saltearlo
    // dejaría la sesión esperando un mensaje que no llega.
    let after_files = started.elapsed();
    let (takes, gives) = dir;
    let mine = if gives {
        crate::merge::changes_for_peer(&local, &remote)
    } else {
        crate::merge::Changes::default()
    };
    let theirs = if takes {
        crate::merge::changes_for_peer(&remote, &local)
    } else {
        crate::merge::Changes::default()
    };

    let applied_here =
        host.with_db(|conn| Ok(crate::merge::apply(conn, &theirs, &music_dir)?))?;
    if applied_here.scope > 0 {
        restore(host);
    }
    sess.send(&Msg::MetaPush {
        changes: Box::new(mine),
    })?;
    let applied_there = match sess.recv()? {
        Msg::MetaAck { applied } => applied,
        other => return Err(anyhow!("expected MetaAck, got {other:?}")),
    };
    // Desglosado y no sólo el total: si un sync repite los mismos números
    // corrida tras corrida, no está convergiendo, y el total solo no dice
    // qué se está re-aplicando.
    log::info!(
        "[sync] here: {} meta, {} playlists, {} memberships, {} deletions, {} scope | there: {} meta, {} playlists, {} memberships, {} deletions, {} scope",
        applied_here.tracks,
        applied_here.playlists,
        applied_here.memberships,
        applied_here.deleted,
        applied_here.scope,
        applied_there.tracks,
        applied_there.playlists,
        applied_there.memberships,
        applied_there.deleted,
        applied_there.scope
    );
    if applied_here.total() > 0 {
        log::debug!(
            "[sync] incoming: {} tracks, {} playlists, {} memberships, {} tombstones",
            theirs.tracks.len(),
            theirs.playlists.len(),
            theirs.memberships.len(),
            theirs.tombstones.len()
        );
    }

    let _ = sess.send(&Msg::Bye);
    let total = started.elapsed();
    let tiempos = format!(
        "[sync] tiempos: inventario {} ms, {total_files} archivo(s) {} ms, organización {} ms, total {} ms",
        after_plan.as_millis(),
        (after_files - after_plan).as_millis(),
        (total - after_files).as_millis(),
        total.as_millis()
    );
    log::info!("{tiempos}");
    // TEMPORAL — al archivo también: en Android con logcat apagado esta línea
    // es la única forma de ver cuánto bloquea un sync.
    crate::perf_line(&tiempos);

    // Historial legible por dispositivo (lo muestra la pantalla de Sync). Sólo
    // las corridas que hicieron algo: una línea por sync automático vacío cada
    // pocos minutos no es historial, es ruido.
    if received + sent + failed + applied_here.total() + applied_there.total() > 0 {
        let _ = host.with_db(|conn| {
            let _ = conn.execute(
                "INSERT INTO sync_log (ts, peer, kind, detail) VALUES (?1, ?2, 'sync', ?3)",
                rusqlite::params![
                    crate::db::now_ms(),
                    peer_uid,
                    format!(
                        "{received} in, {sent} out, {} organized{}",
                        applied_here.total() + applied_there.total(),
                        if failed > 0 { format!(", {failed} failed") } else { String::new() }
                    )
                ],
            );
            Ok(())
        });
    }

    Ok(SyncResult {
        received,
        sent,
        failed,
        bytes,
        organized: applied_here.total() + applied_there.total(),
    })
}

// ---------------------------------------------------------------------------
// Piezas comunes
// ---------------------------------------------------------------------------

/// Datos de un track local por uid: el path real y su entrada de manifest.
fn local_track(
    conn: &Connection,
    uid: &str,
) -> Option<(std::path::PathBuf, crate::manifest::TrackEntry)> {
    conn.query_row(
        "SELECT path, uid, content_hash, COALESCE(size_bytes,0), COALESCE(rel_path,''),
                title, artist, album, genre, duration_ms, bpm, updated_at
         FROM tracks WHERE uid = ?1",
        [uid],
        |r| {
            let path: String = r.get(0)?;
            Ok((
                std::path::PathBuf::from(path),
                crate::manifest::TrackEntry {
                    uid: r.get(1)?,
                    hash: r.get(2)?,
                    size: r.get(3)?,
                    filename: r.get(4)?,
                    title: r.get(5)?,
                    artist: r.get(6)?,
                    album: r.get(7)?,
                    genre: r.get(8)?,
                    duration_ms: r.get(9)?,
                    bpm: r.get(10)?,
                    updated_at: r.get(11)?,
                    present: true,
                },
            ))
        },
    )
    .ok()
}

/// Aplica las filas de scope del manifest del otro lado (LWW). Es el mismo
/// merge que hace `merge::apply` más tarde; acá va antes porque el scope
/// decide qué se transfiere en esta misma corrida.
fn apply_remote_scope<H: Host>(host: &H, remote: &crate::manifest::Manifest) -> Result<()> {
    host.with_db(|conn| {
        for e in &remote.scopes {
            crate::scope::apply_entry(conn, e)?;
        }
        for m in &remote.device_sync {
            crate::scope::apply_device_sync(conn, m)?;
        }
        Ok(())
    })
}

/// Recupera de la papelera lo que haya vuelto a entrar en el scope de este
/// dispositivo. Best-effort: si falla, el archivo se vuelve a bajar por la red
/// y no se pierde nada.
/// El hash se calcula **con el lock soltado**: hashear la papelera puede
/// tardar segundos, y sostener el mutex mientras tanto congela la UI entera —
/// cada `list_tracks` del frontend se queda esperando. Sólo las dos puntas
/// (buscar candidatos, aplicar) tocan la DB, y las dos son baratas.
pub fn restore<H: Host>(host: &H) -> usize {
    let music_dir = host.music_dir();

    // TEMPORAL — diagnóstico. `timed` de afuera mide todo junto, incluida la
    // espera por el lock, y así no se distingue "tarda" de "esperó a otro".
    let t0 = std::time::Instant::now();
    // Averiguar si hay algo que recuperar es una lectura, y casi siempre da
    // cero. Por la conexión de escritura, ese "no hay nada" quedaba encolado
    // detrás del sync: medido en Android, hasta 1255 ms de espera para no hacer
    // nada. El lock de escritura se toma más abajo, y sólo si hay qué mover.
    let candidates = host.with_db_read(|conn| {
        let lock_ms = t0.elapsed().as_millis();
        let t1 = std::time::Instant::now();
        let r = crate::scope::restorable(conn);
        crate::perf_line(&format!(
            "  restore_local: lock {} ms, restorable {} ms, {} candidato(s)",
            lock_ms,
            t1.elapsed().as_millis(),
            r.as_ref().map(|c| c.len()).unwrap_or(0)
        ));
        Ok(r?)
    });
    let candidates = match candidates {
        Ok(c) => c,
        Err(e) => {
            log::warn!("[scope] could not list restorable files: {e}");
            return 0;
        }
    };
    if candidates.is_empty() {
        return 0;
    }

    let t2 = std::time::Instant::now();
    let found = crate::scope::find_in_trash(&music_dir, &candidates);
    crate::perf_line(&format!(
        "  restore_local: find_in_trash {} ms, {} encontrado(s)",
        t2.elapsed().as_millis(),
        found.len()
    ));
    if found.is_empty() {
        return 0;
    }

    let n = host.with_db(|conn| {
        Ok(crate::scope::finish_restore(
            conn,
            &music_dir,
            &found,
            &|p| host.expect_path(p),
        )?)
    });
    let n = match n {
        Ok(n) => n,
        Err(e) => {
            log::warn!("[scope] restoring from trash failed: {e}");
            return 0;
        }
    };
    if n > 0 {
        host.library_changed(true);
    }
    n
}

// ---------------------------------------------------------------------------
// Suite de integridad (Fase 5.8)
// ---------------------------------------------------------------------------
//
// Dos dispositivos completos —cada uno con su base, su carpeta gestionada y su
// papelera— sincronizando de verdad sobre un socket de loopback, con el mismo
// código que corre en la app. Lo que estas pruebas persiguen no es que el
// camino feliz ande (eso ya se ve usándolo): es que **nunca se pierda música**
// en los caminos que no se pueden ensayar a mano sin dos teléfonos y mucha
// paciencia — un corte a la mitad de una transferencia, los dos lados editando
// lo mismo sin verse, un borrado viajando.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{generate_keypair, Session};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::sync::Mutex;
    use std::time::Duration;

    /// Ninguna prueba mueve más de unos megabytes: si algo se queda esperando
    /// es un bug, y vale más un error que una suite colgada.
    const TEST_IO_TIMEOUT: Duration = Duration::from_secs(20);

    /// Un dispositivo de mentira, con todo lo que el motor necesita de uno de
    /// verdad. Es la otra implementación de `Host` (la primera es `AppHandle`).
    struct Device {
        dir: PathBuf,
        db: Mutex<Connection>,
        /// Copia del socket de este lado, para poder cortar la red a mano.
        sock: Mutex<Option<TcpStream>>,
        /// Corta la conexión cuando una transferencia pase estos bytes.
        cut_after: Mutex<Option<u64>>,
        /// Avance visto, para poder afirmar que una reanudación arrancó donde
        /// había quedado y no desde cero.
        seen: Mutex<Vec<(u64, u64)>>,
    }

    impl Host for Device {
        fn with_db<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
            let conn = self.db.lock().map_err(|_| anyhow!("db lock"))?;
            f(&conn)
        }
        fn music_dir(&self) -> PathBuf {
            self.dir.clone()
        }
        fn progress(&self, p: &Progress) {
            self.seen.lock().unwrap().push((p.done, p.total));
            let cut = *self.cut_after.lock().unwrap();
            if let Some(n) = cut {
                if p.done >= n {
                    // Se va la wifi justo acá.
                    if let Some(s) = self.sock.lock().unwrap().as_ref() {
                        let _ = s.shutdown(Shutdown::Both);
                    }
                    *self.cut_after.lock().unwrap() = None;
                }
            }
        }
    }

    impl Device {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "sway-eng-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
            let conn = Connection::open_in_memory().unwrap();
            crate::db::init_schema(&conn).unwrap();
            // Fuerza la identidad: el manifest la necesita y así queda fija.
            crate::db::this_device_uid(&conn).unwrap();
            Self {
                dir,
                db: Mutex::new(conn),
                sock: Mutex::new(None),
                cut_after: Mutex::new(None),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn uid(&self) -> String {
            self.with_db(|c| Ok(crate::db::this_device_uid(c)?)).unwrap()
        }

        /// Un track con archivo real en la carpeta gestionada.
        fn add_track(&self, filename: &str, bytes: &[u8], title: &str) -> String {
            let path = self.dir.join(filename);
            std::fs::write(&path, bytes).unwrap();
            let hash = crate::hashing::hash_file(&path).unwrap();
            let uid = crate::db::new_uid();
            let conn = self.db.lock().unwrap();
            crate::transfer::insert_received(
                &conn,
                &path,
                &uid,
                &hash,
                bytes.len() as u64,
                title,
                "Artista",
                "",
                "",
                0,
                None,
                crate::db::now_ms(),
            )
            .unwrap();
            uid
        }

        fn playlist(&self, name: &str, parent: Option<i64>) -> i64 {
            let conn = self.db.lock().unwrap();
            crate::db::create_playlist(&conn, name, "playlist", parent).unwrap()
        }

        fn folder(&self, name: &str) -> i64 {
            let conn = self.db.lock().unwrap();
            crate::db::create_playlist(&conn, name, "folder", None).unwrap()
        }

        fn track_id(&self, uid: &str) -> i64 {
            let conn = self.db.lock().unwrap();
            conn.query_row("SELECT id FROM tracks WHERE uid = ?1", [uid], |r| r.get(0))
                .unwrap()
        }

        fn add_to_playlist(&self, playlist: i64, track_uids: &[&str]) {
            let ids: Vec<i64> = track_uids.iter().map(|u| self.track_id(u)).collect();
            let mut conn = self.db.lock().unwrap();
            crate::db::add_tracks_to_playlist(&mut conn, playlist, &ids).unwrap();
        }

        /// Borrado local, tal como lo deja `db::delete_tracks`: fila afuera,
        /// tombstone adentro, archivo fuera de la biblioteca. Lo que hace la
        /// app con el archivo (papelera del OS) no es asunto de la red, y en
        /// una prueba mandaría archivos temporales a la papelera de verdad.
        fn delete_track(&self, uid: &str) {
            let conn = self.db.lock().unwrap();
            let path: String = conn
                .query_row("SELECT path FROM tracks WHERE uid = ?1", [uid], |r| r.get(0))
                .unwrap();
            conn.execute("DELETE FROM tracks WHERE uid = ?1", [uid]).unwrap();
            crate::db::record_tombstone(&conn, "track", uid).unwrap();
            std::fs::remove_file(path).ok();
        }

        fn track_uids(&self) -> Vec<String> {
            let conn = self.db.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT uid FROM tracks ORDER BY uid")
                .unwrap();
            let v: Vec<String> = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            v
        }

        /// Títulos de una playlist, en el orden que define el rank.
        fn order_in(&self, playlist: &str) -> Vec<String> {
            let conn = self.db.lock().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT t.title FROM playlist_tracks pt
                     JOIN playlists p ON p.id = pt.playlist_id
                     JOIN tracks t ON t.id = pt.track_id
                     WHERE p.name = ?1 ORDER BY pt.rank",
                )
                .unwrap();
            let v: Vec<String> = stmt
                .query_map([playlist], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            v
        }

        fn playlist_names(&self) -> Vec<String> {
            let conn = self.db.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT name FROM playlists ORDER BY name")
                .unwrap();
            let v: Vec<String> = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            v
        }

        /// Archivos de audio de la carpeta gestionada (sin papelera ni
        /// parciales, que viven en subdirectorios `.sway-*`).
        fn audio_files(&self) -> Vec<PathBuf> {
            let mut v: Vec<PathBuf> = std::fs::read_dir(&self.dir)
                .unwrap()
                .flatten()
                .filter(|e| e.path().is_file())
                .map(|e| e.path())
                .collect();
            v.sort();
            v
        }

        /// Todo lo que quedó en la papelera de la biblioteca.
        fn trashed(&self) -> Vec<Vec<u8>> {
            let dir = crate::trash::trash_dir(&self.dir);
            std::fs::read_dir(dir)
                .map(|rd| {
                    rd.flatten()
                        .filter(|e| e.path().is_file())
                        .map(|e| std::fs::read(e.path()).unwrap())
                        .collect()
                })
                .unwrap_or_default()
        }

        fn partials(&self) -> Vec<PathBuf> {
            std::fs::read_dir(crate::transfer::incoming_dir(&self.dir))
                .map(|rd| rd.flatten().map(|e| e.path()).collect())
                .unwrap_or_default()
        }

        fn cut_after(&self, bytes: u64) {
            *self.cut_after.lock().unwrap() = Some(bytes);
        }

        fn first_progress(&self) -> u64 {
            self.seen.lock().unwrap().first().map(|(d, _)| *d).unwrap_or(0)
        }

        fn forget_progress(&self) {
            self.seen.lock().unwrap().clear();
        }
    }

    impl Drop for Device {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    /// Las dos puntas de un canal cifrado sobre loopback, con las copias de los
    /// sockets anotadas en cada dispositivo para poder cortar la red.
    fn link(a: &Device, b: &Device) -> (Session, Session) {
        let (ka, _) = generate_keypair().unwrap();
        let (kb, _) = generate_keypair().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(TEST_IO_TIMEOUT)).unwrap();
            let clone = s.try_clone().unwrap();
            (Session::accept(s, &kb).unwrap(), clone)
        });
        let cs = TcpStream::connect(addr).unwrap();
        cs.set_read_timeout(Some(TEST_IO_TIMEOUT)).unwrap();
        let mine = cs.try_clone().unwrap();
        let client = Session::connect(cs, &ka).unwrap();
        let (srv, theirs) = server.join().unwrap();
        *a.sock.lock().unwrap() = Some(mine);
        *b.sock.lock().unwrap() = Some(theirs);
        (client, srv)
    }

    /// Una corrida completa: `a` sincroniza, `b` atiende. Es exactamente lo que
    /// pasa entre la PC y el celular, con el hilo del servidor de este lado.
    fn sync_once(a: &Device, b: &Device) -> Result<SyncResult> {
        let (mut ca, mut sb) = link(a, b);
        let a_uid = a.uid();
        let b_uid = b.uid();
        std::thread::scope(|s| {
            let server = s.spawn(move || serve_requests(b, &mut sb, &a_uid));
            let out = sync(a, &mut ca, &b_uid);
            let _ = server.join().unwrap();
            out
        })
    }

    fn audio(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| ((i + seed as usize) % 251) as u8).collect()
    }

    // -----------------------------------------------------------------------

    /// El caso base, y la propiedad que más se rompió en device: converger una
    /// vez es fácil, quedarse quieto después es lo difícil. Un sync que repite
    /// trabajo en cada corrida es el síntoma de todos los bugs de ping-pong de
    /// la Fase 5.
    #[test]
    fn dos_bibliotecas_convergen_y_la_segunda_corrida_no_mueve_nada() {
        let a = Device::new("conv-a");
        let b = Device::new("conv-b");
        let t1 = a.add_track("uno.flac", &audio(2048, 1), "Uno");
        let t2 = a.add_track("dos.flac", &audio(3000, 2), "Dos");
        a.add_track("tres.flac", &audio(1500, 3), "Tres");
        let gigs = a.folder("Gigs");
        let set = a.playlist("Set", Some(gigs));
        // Al revés del orden de importación: el orden manual es dato, no un
        // efecto secundario de cómo entraron los archivos.
        a.add_to_playlist(set, &[&t2, &t1]);

        let r = sync_once(&a, &b).expect("el primer sync tiene que andar");
        assert_eq!(r.sent, 3, "los tres archivos viajan");
        assert_eq!(r.received, 0);

        assert_eq!(a.track_uids(), b.track_uids(), "misma identidad de los dos lados");
        assert_eq!(b.audio_files().len(), 3);
        assert_eq!(b.playlist_names(), vec!["Gigs".to_string(), "Set".to_string()]);
        assert_eq!(b.order_in("Set"), vec!["Dos".to_string(), "Uno".to_string()]);
        // La jerarquía viaja por uid, no por id local.
        let parent: String = {
            let conn = b.db.lock().unwrap();
            conn.query_row(
                "SELECT parent.name FROM playlists p JOIN playlists parent ON parent.id = p.parent_id
                 WHERE p.name = 'Set'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(parent, "Gigs");

        let r2 = sync_once(&a, &b).expect("la segunda corrida tiene que andar");
        assert_eq!(
            (r2.sent, r2.received, r2.organized),
            (0, 0, 0),
            "ya está todo: no puede volver a mover nada"
        );
        // Y tampoco al revés: si el otro lado creyera que le falta algo, el
        // par quedaría mandándose lo mismo para siempre.
        let r3 = sync_once(&b, &a).expect("la vuelta también");
        assert_eq!((r3.sent, r3.received, r3.organized), (0, 0, 0));
    }

    /// El caso que no se puede probar a mano sin pelearse con la wifi: la red
    /// se corta a la mitad de un archivo. No se pierde nada, y la corrida
    /// siguiente **reanuda** en vez de bajar todo de nuevo.
    #[test]
    fn un_corte_a_la_mitad_no_pierde_nada_y_la_reanudacion_sigue_donde_iba() {
        let a = Device::new("cut-a");
        let b = Device::new("cut-b");
        let bytes = audio(2_500_000, 7);
        a.add_track("largo.flac", &bytes, "Largo");

        // Lo pide B, así que es B quien reanuda: el offset lo lleva el pedido.
        b.cut_after(1_000_000);
        let err = sync_once(&b, &a).expect_err("la red se cortó, el sync no puede decir que anduvo");
        assert!(is_disconnect(&err), "un corte es un corte, no un error raro: {err}");

        assert!(b.track_uids().is_empty(), "nada a medio bajar entra a la biblioteca");
        assert!(b.audio_files().is_empty(), "y no queda un archivo trucho en la carpeta");
        let partials = b.partials();
        assert_eq!(partials.len(), 1, "lo bajado sobrevive como parcial");
        let partial_len = std::fs::metadata(&partials[0]).unwrap().len();
        assert!(
            partial_len >= 1_000_000 && partial_len < bytes.len() as u64,
            "el parcial tiene lo que llegó ({partial_len} bytes)"
        );
        assert_eq!(a.audio_files().len(), 1, "el que envía no pierde nada nunca");

        b.forget_progress();
        let r = sync_once(&b, &a).expect("y ahora sí");
        assert_eq!(r.received, 1);
        assert!(
            b.first_progress() >= 1_000_000,
            "arrancó de cero: la reanudación no sirvió de nada"
        );
        assert_eq!(b.audio_files().len(), 1, "un archivo, no dos");
        assert_eq!(std::fs::read(&b.audio_files()[0]).unwrap(), bytes, "y es el mismo");
        assert!(b.partials().is_empty(), "el parcial se consume al terminar");
    }

    /// Un borrado viaja, pero el archivo **no se destruye**: queda en la
    /// papelera de la biblioteca 30 días. Y no vuelve solo en la corrida
    /// siguiente, que es lo que haría un merge ingenuo por unión.
    #[test]
    fn un_borrado_viaja_deja_el_archivo_en_la_papelera_y_no_resucita() {
        let a = Device::new("del-a");
        let b = Device::new("del-b");
        let bytes = audio(4096, 11);
        let uid = a.add_track("chau.flac", &bytes, "Chau");
        a.add_track("queda.flac", &audio(2048, 12), "Queda");
        sync_once(&a, &b).unwrap();
        assert_eq!(b.audio_files().len(), 2);

        a.delete_track(&uid);
        sync_once(&a, &b).unwrap();

        assert_eq!(b.track_uids().len(), 1, "el borrado se aplicó del otro lado");
        assert_eq!(b.audio_files().len(), 1);
        assert!(
            b.trashed().contains(&bytes),
            "pero el archivo sigue existiendo, en la papelera"
        );

        let r = sync_once(&a, &b).unwrap();
        assert_eq!((r.sent, r.received), (0, 0), "lo borrado no puede volver");
        assert_eq!(a.track_uids().len(), 1);
        assert_eq!(b.track_uids().len(), 1);
    }

    /// Los dos lados editan sin verse. La regla es la misma de siempre: gana
    /// el más nuevo campo por campo, y ante duda se conserva — las membresías
    /// se unen, no se pisan.
    #[test]
    fn editar_los_dos_lados_sin_verse_no_pierde_ninguno_de_los_dos_cambios() {
        let a = Device::new("split-a");
        let b = Device::new("split-b");
        let t1 = a.add_track("uno.flac", &audio(2048, 31), "Uno");
        let t2 = a.add_track("dos.flac", &audio(2048, 32), "Dos");
        let set = a.playlist("Set", None);
        a.add_to_playlist(set, &[&t1]);
        sync_once(&a, &b).unwrap();

        // Sin verse: cada uno le cambia el nombre a la misma playlist y le
        // agrega algo distinto.
        {
            let conn = b.db.lock().unwrap();
            let id: i64 = conn
                .query_row("SELECT id FROM playlists WHERE name = 'Set'", [], |r| r.get(0))
                .unwrap();
            // El de B es el cambio VIEJO: se hizo antes de que se vieran.
            conn.execute(
                "UPDATE playlists SET name = 'Sábado', updated_at = ?1 WHERE id = ?2",
                rusqlite::params![crate::db::now_ms() - 10_000, id],
            )
            .unwrap();
        }
        let warmup = b.playlist("Warmup", None);
        let b_t2 = b.track_uids().into_iter().find(|u| *u == t2).unwrap();
        b.add_to_playlist(warmup, &[&b_t2]);

        {
            let conn = a.db.lock().unwrap();
            crate::db::rename_playlist(&conn, set, "Viernes").unwrap();
        }
        a.add_to_playlist(set, &[&t2]);

        sync_once(&a, &b).unwrap();

        for (quien, d) in [("A", &a), ("B", &b)] {
            let mut nombres = d.playlist_names();
            nombres.sort();
            assert_eq!(
                nombres,
                vec!["Viernes".to_string(), "Warmup".to_string()],
                "{quien}: gana el rename más nuevo y la playlist nueva del otro no se pierde"
            );
            let mut orden = d.order_in("Viernes");
            orden.sort();
            assert_eq!(
                orden,
                vec!["Dos".to_string(), "Uno".to_string()],
                "{quien}: las membresías se unen"
            );
            assert_eq!(d.order_in("Warmup"), vec!["Dos".to_string()], "{quien}: y la de la playlist nueva también");
        }

        let r = sync_once(&a, &b).unwrap();
        assert_eq!(
            (r.sent, r.received, r.organized),
            (0, 0, 0),
            "después de resolver el conflicto tienen que quedarse quietos"
        );
    }

    /// El ciclo completo de la sync selectiva, que es donde más fácil se
    /// perdería música: desmarcar una playlist, liberar el espacio, y volver a
    /// marcarla. Las tres cosas que tienen que valer son que liberar **no
    /// destruye** (va a la papelera), que lo liberado no vuelve solo mientras
    /// esté fuera de scope, y que al re-marcarlo se rescata del disco en vez de
    /// bajarlo de nuevo por la red — que con una biblioteca de 20 GB no es un
    /// detalle de eficiencia sino la diferencia entre usable y no usable.
    #[test]
    fn liberar_espacio_no_destruye_y_volver_a_marcar_rescata_sin_red() {
        let a = Device::new("scope-a");
        let b = Device::new("scope-b");
        let fiesta_bytes = audio(4096, 41);
        let t1 = a.add_track("set.flac", &audio(2048, 42), "DelSet");
        let t2 = a.add_track("fiesta.flac", &fiesta_bytes, "DeLaFiesta");
        let set = a.playlist("Set", None);
        let fiesta = a.playlist("Fiesta", None);
        a.add_to_playlist(set, &[&t1]);
        a.add_to_playlist(fiesta, &[&t2]);

        // Lo inicia B: así queda anotado que A tiene copia de los dos archivos,
        // que es lo que después habilita liberar espacio sin riesgo.
        let r = sync_once(&b, &a).unwrap();
        assert_eq!(r.received, 2);

        // B pasa a selectivo y se queda sólo con "Set".
        let b_uid = b.uid();
        let fiesta_uid: String = {
            let conn = b.db.lock().unwrap();
            crate::scope::set_mode(&conn, &b_uid, crate::scope::Mode::Selected).unwrap();
            let s: String = conn
                .query_row("SELECT uid FROM playlists WHERE name = 'Set'", [], |r| r.get(0))
                .unwrap();
            let f: String = conn
                .query_row("SELECT uid FROM playlists WHERE name = 'Fiesta'", [], |r| r.get(0))
                .unwrap();
            crate::scope::set_playlist(&conn, &b_uid, &s, true).unwrap();
            f
        };

        // Liberar espacio: sólo lo que consta en otro dispositivo.
        let (n, _) = {
            let conn = b.db.lock().unwrap();
            let items = crate::scope::evictable(&conn, &b.dir).unwrap();
            assert_eq!(items.len(), 1, "sólo el que quedó fuera de scope");
            crate::scope::evict(&conn, &b.dir, &items).unwrap()
        };
        assert_eq!(n, 1);
        assert_eq!(b.audio_files().len(), 1, "el archivo salió de la biblioteca");
        assert!(b.trashed().contains(&fiesta_bytes), "pero está en la papelera, no destruido");
        assert_eq!(b.track_uids().len(), 2, "y la fila se queda: el track se sigue viendo");

        // Fuera de scope no vuelve solo, por más que el otro lo tenga.
        let r = sync_once(&b, &a).unwrap();
        assert_eq!(r.received, 0, "desmarcado es desmarcado");
        assert_eq!(b.audio_files().len(), 1);

        // Se vuelve a marcar: el archivo tiene que salir de la papelera.
        {
            let conn = b.db.lock().unwrap();
            crate::scope::set_playlist(&conn, &b_uid, &fiesta_uid, true).unwrap();
        }
        let r = sync_once(&b, &a).unwrap();
        assert_eq!(
            r.received, 0,
            "estaba a un rename de distancia: no puede haber viajado por la red"
        );
        assert_eq!(b.audio_files().len(), 2, "y sin embargo volvió");
        let vuelto = b
            .audio_files()
            .into_iter()
            .map(|p| std::fs::read(p).unwrap())
            .any(|c| c == fiesta_bytes);
        assert!(vuelto, "con los mismos bytes, verificados por hash");
    }
}
