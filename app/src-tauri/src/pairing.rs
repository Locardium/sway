//! Vinculación de dispositivos (Fase 5.2).
//!
//! El handshake Noise deja un canal cifrado con *alguien*; el pairing es lo
//! que lo convierte en un canal cifrado con *este dispositivo*. Los dos lados
//! muestran el mismo código de 6 dígitos y el usuario confirma en ambas
//! pantallas — recién ahí se fija la clave pública del otro en `devices`.
//!
//! Reglas duras:
//! - El pairing necesita que **los dos** acepten. Que uno solo vea un código
//!   distinto alcanza para cortar.
//! - Una clave distinta para un uid ya conocido **se rechaza y se avisa**.
//!   Nunca se vuelve a confiar en silencio: eso sería exactamente lo que
//!   haría un intermediario para hacerse pasar por un dispositivo tuyo.
//! - Un dispositivo sin parear no recibe ningún dato de la biblioteca. El
//!   `Hello` con los conteos va después del pairing, nunca antes.

use crate::db;
use crate::wire::{Msg, Session};
use crate::AppState;
use anyhow::{anyhow, Result};
use base64::Engine;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{channel, Sender};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

/// Cuánto se espera a que una persona mire la pantalla y confirme.
const DECISION_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Generoso a propósito: del otro lado puede haber alguien todavía decidiendo.
const IO_TIMEOUT: Duration = Duration::from_secs(180);

const SETTING_PRIVKEY: &str = "noise_private";
const SETTING_PUBKEY: &str = "noise_public";

/// Decisiones de pairing pendientes, por uid del peer. El hilo de la conexión
/// espera en el receptor; el comando `confirm_pairing` manda la respuesta.
#[derive(Default)]
pub struct Pairing {
    pending: Mutex<HashMap<String, Sender<bool>>>,
    /// Syncs en curso, por uid. Con el sync automático hay varios
    /// disparadores (cambio local, peer que aparece, periódico) que pueden
    /// caer casi juntos: dos corridas simultáneas contra el mismo
    /// dispositivo se pisarían los archivos a medio bajar.
    active: Mutex<HashSet<String>>,
}

/// Marca un sync en curso; al soltarse lo desmarca pase lo que pase.
struct SyncGuard(AppHandle, String);

impl SyncGuard {
    fn acquire(handle: &AppHandle, uid: &str) -> Option<Self> {
        let state = handle.state::<AppState>();
        let mut active = state.pairing.active.lock().ok()?;
        if !active.insert(uid.to_string()) {
            return None; // ya hay uno corriendo con este peer
        }
        Some(SyncGuard(handle.clone(), uid.to_string()))
    }
}

impl Drop for SyncGuard {
    fn drop(&mut self) {
        let state = self.0.state::<AppState>();
        // El guard va a una variable propia: como binding del `if let` sería
        // un temporario que vive más que `state`, y no compila.
        let active = state.pairing.active.lock();
        if let Ok(mut active) = active {
            active.remove(&self.1);
        }
    }
}

impl Pairing {
    fn resolve(&self, uid: &str, accepted: bool) -> bool {
        match self.pending.lock().unwrap().remove(uid) {
            Some(tx) => tx.send(accepted).is_ok(),
            None => false,
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PairingRequestEvent {
    uid: String,
    name: String,
    platform: String,
    code: String,
    /// `true` si el otro dispositivo inició el pairing.
    incoming: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PairingDoneEvent {
    uid: String,
    name: String,
    ok: bool,
    error: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerHelloEvent {
    pub uid: String,
    pub name: String,
    pub tracks: i64,
    pub playlists: i64,
    /// Diferencia de reloj con el otro dispositivo, en ms. Importa para el
    /// merge por LWW de 5.5: con relojes corridos, "el último gana" elige mal.
    pub clock_skew_ms: i64,
}

// ---------------------------------------------------------------------------
// Identidad criptográfica de este dispositivo
// ---------------------------------------------------------------------------

/// Par de claves estático, generado una sola vez. Vive en `app_settings`, o
/// sea en la DB de la app (en Android, almacenamiento privado del paquete).
fn keypair(conn: &rusqlite::Connection) -> Result<(Vec<u8>, Vec<u8>)> {
    let b64 = base64::engine::general_purpose::STANDARD;
    if let (Some(priv_b64), Some(pub_b64)) = (
        db::get_setting(conn, SETTING_PRIVKEY)?,
        db::get_setting(conn, SETTING_PUBKEY)?,
    ) {
        if let (Ok(pv), Ok(pb)) = (b64.decode(&priv_b64), b64.decode(&pub_b64)) {
            return Ok((pv, pb));
        }
    }
    let (private, public) = crate::wire::generate_keypair()?;
    db::set_setting(conn, SETTING_PRIVKEY, &b64.encode(&private))?;
    db::set_setting(conn, SETTING_PUBKEY, &b64.encode(&public))?;
    log::info!("[pair] par de claves nuevo generado");
    Ok((private, public))
}

fn private_key(handle: &AppHandle) -> Result<Vec<u8>> {
    let state = handle.state::<AppState>();
    let conn = state.db.lock().map_err(|_| anyhow!("db lock"))?;
    Ok(keypair(&conn)?.0)
}

// ---------------------------------------------------------------------------
// Estado de `devices`
// ---------------------------------------------------------------------------

enum Known {
    /// Ya pareado y la clave coincide.
    Trusted,
    /// Nunca se pareó con este uid.
    Unknown,
    /// Conocido pero con OTRA clave pública. Alarma, no rutina.
    KeyMismatch,
}

fn known_state(handle: &AppHandle, uid: &str, pubkey: &[u8]) -> Known {
    let state = handle.state::<AppState>();
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(_) => return Known::Unknown,
    };
    let stored: Option<Option<Vec<u8>>> = conn
        .query_row("SELECT pubkey FROM devices WHERE uid = ?1", [uid], |r| r.get(0))
        .ok();
    match stored {
        Some(Some(k)) if k == pubkey => Known::Trusted,
        Some(Some(_)) => Known::KeyMismatch,
        _ => Known::Unknown,
    }
}

fn store_device(handle: &AppHandle, uid: &str, name: &str, platform: &str, pubkey: &[u8]) -> Result<()> {
    let state = handle.state::<AppState>();
    let conn = state.db.lock().map_err(|_| anyhow!("db lock"))?;
    let now = db::now_ms();
    conn.execute(
        "INSERT INTO devices (uid, name, platform, pubkey, paired_at, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)
         ON CONFLICT(uid) DO UPDATE SET
            name = excluded.name, platform = excluded.platform,
            pubkey = excluded.pubkey, paired_at = excluded.paired_at,
            last_seen = excluded.last_seen",
        rusqlite::params![uid, name, platform, pubkey, now],
    )?;
    // Política por defecto. Todavía no hace nada (5.6/5.7 la usan), pero la
    // fila tiene que existir desde el pairing para que la UI pueda editarla.
    conn.execute(
        "INSERT OR IGNORE INTO sync_policy (device_uid) VALUES (?1)",
        [uid],
    )?;
    conn.execute(
        "INSERT INTO sync_log (ts, peer, kind, detail) VALUES (?1, ?2, 'paired', ?3)",
        rusqlite::params![now, uid, name],
    )?;
    Ok(())
}

fn library_counts(handle: &AppHandle) -> (i64, i64) {
    let state = handle.state::<AppState>();
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(_) => return (0, 0),
    };
    let tracks = conn
        .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
        .unwrap_or(0);
    let playlists = conn
        .query_row("SELECT COUNT(*) FROM playlists WHERE kind = 'playlist'", [], |r| r.get(0))
        .unwrap_or(0);
    (tracks, playlists)
}

fn me(handle: &AppHandle) -> Result<(String, String)> {
    let state = handle.state::<AppState>();
    let conn = state.db.lock().map_err(|_| anyhow!("db lock"))?;
    let uid = db::this_device_uid(&conn)?;
    let name = db::device_name(&conn)?;
    Ok((uid, name))
}

fn platform() -> String {
    if cfg!(target_os = "android") {
        "android".into()
    } else if cfg!(target_os = "windows") {
        "windows".into()
    } else if cfg!(target_os = "macos") {
        "macos".into()
    } else {
        "linux".into()
    }
}

// ---------------------------------------------------------------------------
// Confirmación del usuario
// ---------------------------------------------------------------------------

/// Muestra el código y espera a que la persona decida. El timeout evita que
/// un hilo (y una conexión) queden colgados si nadie mira la pantalla.
fn ask_user(handle: &AppHandle, ev: PairingRequestEvent) -> bool {
    let (tx, rx) = channel();
    {
        let state = handle.state::<AppState>();
        state.pairing.pending.lock().unwrap().insert(ev.uid.clone(), tx);
    }
    let uid = ev.uid.clone();
    let _ = handle.emit("pairing-request", ev);
    let answer = rx.recv_timeout(DECISION_TIMEOUT).unwrap_or(false);
    let state = handle.state::<AppState>();
    state.pairing.pending.lock().unwrap().remove(&uid);
    answer
}

/// Lo llama el comando `confirm_pairing` desde la UI.
pub fn resolve_decision(handle: &AppHandle, uid: &str, accepted: bool) -> bool {
    handle.state::<AppState>().pairing.resolve(uid, accepted)
}

// ---------------------------------------------------------------------------
// Lado que acepta conexiones
// ---------------------------------------------------------------------------

/// Escucha en el puerto que 5.1 reservó y anunció por mDNS.
pub fn spawn_server(handle: AppHandle, listener: TcpListener) {
    std::thread::spawn(move || {
        log::info!("[pair] escuchando en {:?}", listener.local_addr());
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("[pair] accept fallo: {e}");
                    continue;
                }
            };
            let handle = handle.clone();
            std::thread::spawn(move || {
                if let Err(e) = serve(&handle, stream) {
                    // Los sondeos de alcanzabilidad (ver discovery::spawn_prober)
                    // conectan y cortan sin mandar nada: es trafico esperado,
                    // no un error que valga la pena reportar cada 10 segundos.
                    if is_disconnect(&e) {
                        log::debug!("[pair] sondeo de alcanzabilidad");
                    } else {
                        log::warn!("[pair] conexion entrante terminada: {e}");
                    }
                }
            });
        }
    });
}

/// Después de presentarse, la sesión queda abierta atendiendo pedidos hasta
/// que el otro corta. Por ahora el único es el manifest; 5.4 agrega los
/// bloques de audio sobre la misma sesión, para no rehacer el handshake por
/// cada archivo.
fn serve_requests(handle: &AppHandle, sess: &mut Session, peer_uid: &str) -> Result<()> {
    loop {
        let msg = match sess.recv() {
            Ok(m) => m,
            // Cortó: fin normal de la sesión, no un error.
            Err(e) if is_disconnect(&e) => return Ok(()),
            Err(e) => return Err(e),
        };
        match msg {
            Msg::ManifestReq => {
                let manifest = {
                    let state = handle.state::<AppState>();
                    let conn = state.db.lock().map_err(|_| anyhow!("db lock"))?;
                    crate::manifest::build(&conn)?
                };
                sess.send(&Msg::ManifestData {
                    manifest: Box::new(manifest),
                })?;
            }
            // Alguien pide un archivo nuestro.
            Msg::BlobReq { hash, offset } => {
                let path = {
                    let state = handle.state::<AppState>();
                    let conn = state.db.lock().map_err(|_| anyhow!("db lock"))?;
                    crate::transfer::path_for_hash(&conn, &hash)
                };
                match path {
                    Some(p) => crate::transfer::send_file(sess, &p, offset, &hash)?,
                    None => sess.send(&Msg::BlobError {
                        reason: format!("no tengo el archivo {hash}"),
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
                let music_dir = handle.state::<AppState>().music_dir.clone();
                let handle2 = handle.clone();
                let got = crate::transfer::receive_file(
                    sess,
                    &music_dir,
                    &hash,
                    &filename,
                    &mut |_, _| {},
                    &mut |dest| handle2.state::<AppState>().expect_path(dest),
                )?;
                {
                    let state = handle.state::<AppState>();
                    let conn = state.db.lock().map_err(|_| anyhow!("db lock"))?;
                    crate::transfer::insert_received(
                        &conn,
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
                    )?;
                }
                log::info!("[sync] recibido {filename} ({} bytes)", got.bytes);
                let _ = handle.emit("library-changed", ());
            }
            // Cambios de organización que nos manda el otro lado.
            Msg::MetaPush { changes } => {
                let applied = {
                    let state = handle.state::<AppState>();
                    let music_dir = state.music_dir.clone();
                    let conn = state.db.lock().map_err(|_| anyhow!("db lock"))?;
                    let policy = delete_policy(&conn, peer_uid);
                    crate::merge::apply(&conn, &changes, &music_dir, policy)?
                };
                if applied.total() > 0 {
                    log::info!(
                        "[sync] aplicados {} tracks, {} playlists, {} membresías",
                        applied.tracks,
                        applied.playlists,
                        applied.memberships
                    );
                    let _ = handle.emit("library-changed", ());
                }
                sess.send(&Msg::MetaAck { applied })?;
            }
            Msg::Bye => return Ok(()),
            other => return Err(anyhow!("pedido inesperado: {other:?}")),
        }
    }
}

/// El otro lado cerró sin decir nada: un sondeo, o una app que se fue.
fn is_disconnect(e: &anyhow::Error) -> bool {
    use std::io::ErrorKind::*;
    e.downcast_ref::<std::io::Error>()
        .map(|io| matches!(io.kind(), UnexpectedEof | ConnectionReset | ConnectionAborted))
        .unwrap_or(false)
}

fn serve(handle: &AppHandle, stream: TcpStream) -> Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let private = private_key(handle)?;
    let mut sess = Session::accept(stream, &private)?;

    match sess.recv()? {
        Msg::PairRequest { uid, name, platform } => {
            match known_state(handle, &uid, &sess.peer_pubkey) {
                Known::KeyMismatch => {
                    let _ = sess.send(&Msg::Reject {
                        reason: "clave distinta a la que ya tenías para este dispositivo".into(),
                    });
                    warn_key_mismatch(handle, &uid, &name);
                    return Err(anyhow!("clave distinta para {uid}"));
                }
                Known::Trusted | Known::Unknown => {}
            }

            let accepted_here = ask_user(
                handle,
                PairingRequestEvent {
                    uid: uid.clone(),
                    name: name.clone(),
                    platform: platform.clone(),
                    code: sess.code.clone(),
                    incoming: true,
                },
            );
            sess.send(&Msg::PairResponse {
                accepted: accepted_here,
            })?;
            if !accepted_here {
                emit_done(handle, &uid, &name, false, Some("rechazado en este dispositivo"));
                return Ok(());
            }
            // El otro lado también tiene que haber aceptado.
            let accepted_there = match sess.recv()? {
                Msg::PairAck { accepted } => accepted,
                Msg::Reject { reason } => {
                    emit_done(handle, &uid, &name, false, Some(&reason));
                    return Ok(());
                }
                other => return Err(anyhow!("se esperaba PairAck, llego {other:?}")),
            };
            if !accepted_there {
                emit_done(handle, &uid, &name, false, Some("rechazado en el otro dispositivo"));
                return Ok(());
            }
            store_device(handle, &uid, &name, &platform, &sess.peer_pubkey)?;
            emit_done(handle, &uid, &name, true, None);
            let _ = handle.emit("peers-changed", ());
            exchange_hello(handle, &mut sess, &uid, &name)
        }
        Msg::Hello {
            uid,
            name,
            tracks,
            playlists,
            clock_ms,
            ..
        } => {
            match known_state(handle, &uid, &sess.peer_pubkey) {
                Known::Trusted => {}
                Known::KeyMismatch => {
                    let _ = sess.send(&Msg::Reject {
                        reason: "clave distinta a la que ya tenías para este dispositivo".into(),
                    });
                    warn_key_mismatch(handle, &uid, &name);
                    return Err(anyhow!("clave distinta para {uid}"));
                }
                Known::Unknown => {
                    // No es un error del otro lado: probablemente lo
                    // desvinculamos nosotros. Que se entere y limpie su fila.
                    let _ = sess.send(&Msg::NotPaired);
                    return Ok(());
                }
            }
            let (my_uid, my_name) = me(handle)?;
            let (my_tracks, my_playlists) = library_counts(handle);
            sess.send(&Msg::Hello {
                uid: my_uid,
                name: my_name,
                platform: platform(),
                tracks: my_tracks,
                playlists: my_playlists,
                clock_ms: db::now_ms(),
            })?;
            report_hello(handle, &uid, &name, tracks, playlists, clock_ms);
            serve_requests(handle, &mut sess, &uid)
        }
        // El otro lado nos sacó de sus dispositivos. Solo se acepta si su
        // clave es la que teníamos guardada — o sea, si el handshake probó
        // que es realmente él y no alguien pidiendo que nos desvinculemos.
        Msg::Unpair { uid } => {
            match known_state(handle, &uid, &sess.peer_pubkey) {
                Known::Trusted => {
                    forget_device(handle, &uid)?;
                    log::info!("[pair] {uid} nos desvinculó");
                    let _ = handle.emit("peers-changed", ());
                    Ok(())
                }
                _ => Err(anyhow!("unpair de un peer que no está vinculado ({uid})")),
            }
        }
        other => Err(anyhow!("primer mensaje inesperado: {other:?}")),
    }
}

// ---------------------------------------------------------------------------
// Lado que llama
// ---------------------------------------------------------------------------

/// Conecta con un peer: lo parea si hace falta, y si ya está pareado
/// intercambia `Hello`. Corre en su propio hilo — del otro lado puede haber
/// una persona tardando en confirmar.
pub fn connect_peer(handle: AppHandle, uid: String) {
    std::thread::spawn(move || {
        let name = peer_name(&handle, &uid);
        if let Err(e) = connect_inner(&handle, &uid) {
            log::warn!("[pair] conexion con {uid} fallo: {e}");
            // Solo los fallos de RED apagan la fila: que diga "conectado"
            // justo después de un timeout es la peor combinación posible.
            // Un rechazo lógico (no vinculado, clave distinta) no significa
            // que el dispositivo no esté ahí — pintarlo de gris sería mentir
            // igual, en la otra dirección.
            if e.downcast_ref::<std::io::Error>().is_some() {
                let state = handle.state::<AppState>();
                if state.peers.mark_unreachable(&uid) {
                    let _ = handle.emit("peers-changed", ());
                }
            }
            emit_done(&handle, &uid, &name, false, Some(&e.to_string()));
        }
    });
}

fn peer_name(handle: &AppHandle, uid: &str) -> String {
    handle
        .state::<AppState>()
        .peers
        .list()
        .into_iter()
        .find(|p| p.uid == uid)
        .map(|p| p.name)
        .unwrap_or_else(|| uid.to_string())
}

fn peer_addr(handle: &AppHandle, uid: &str) -> Result<SocketAddr> {
    let peer = handle
        .state::<AppState>()
        .peers
        .list()
        .into_iter()
        .find(|p| p.uid == uid)
        .ok_or_else(|| anyhow!("el dispositivo ya no está visible en la red"))?;
    let addr = peer
        .addrs
        .first()
        .ok_or_else(|| anyhow!("el dispositivo no publicó ninguna dirección"))?;
    format!("{addr}:{}", peer.port)
        .parse()
        .map_err(|e| anyhow!("dirección inválida ({addr}): {e}"))
}

fn connect_inner(handle: &AppHandle, uid: &str) -> Result<()> {
    let addr = peer_addr(handle, uid)?;
    let name = peer_name(handle, uid);
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let private = private_key(handle)?;
    let mut sess = Session::connect(stream, &private)?;

    let (my_uid, my_name) = me(handle)?;
    match known_state(handle, uid, &sess.peer_pubkey) {
        Known::KeyMismatch => {
            warn_key_mismatch(handle, uid, &name);
            return Err(anyhow!(
                "la clave de {name} no coincide con la que tenías guardada"
            ));
        }
        Known::Trusted => {
            let (tracks, playlists) = library_counts(handle);
            sess.send(&Msg::Hello {
                uid: my_uid,
                name: my_name,
                platform: platform(),
                tracks,
                playlists,
                clock_ms: db::now_ms(),
            })?;
            match sess.recv()? {
                Msg::Hello {
                    uid: their_uid,
                    name: their_name,
                    tracks,
                    playlists,
                    clock_ms,
                    ..
                } => {
                    report_hello(handle, &their_uid, &their_name, tracks, playlists, clock_ms);
                    Ok(())
                }
                // Nos desvincularon del otro lado. Es creíble: el handshake ya
                // probó que la clave es la que teníamos guardada. Seguir
                // mostrando "Paired" sería mentir.
                Msg::NotPaired => {
                    forget_device(handle, uid)?;
                    let _ = handle.emit("peers-changed", ());
                    Err(anyhow!("{name} ya no te tiene vinculado"))
                }
                Msg::Reject { reason } => Err(anyhow!(reason)),
                other => Err(anyhow!("se esperaba Hello, llego {other:?}")),
            }
        }
        Known::Unknown => {
            sess.send(&Msg::PairRequest {
                uid: my_uid,
                name: my_name,
                platform: platform(),
            })?;
            let accepted_here = ask_user(
                handle,
                PairingRequestEvent {
                    uid: uid.to_string(),
                    name: name.clone(),
                    platform: String::new(),
                    code: sess.code.clone(),
                    incoming: false,
                },
            );
            if !accepted_here {
                // Cortar acá y no esperar la respuesta del otro: puede haber
                // alguien mirando la pantalla hasta que expire el timeout.
                let _ = sess.send(&Msg::Reject {
                    reason: "rechazado en el otro dispositivo".into(),
                });
                emit_done(handle, uid, &name, false, Some("rechazado en este dispositivo"));
                return Ok(());
            }
            let accepted_there = match sess.recv()? {
                Msg::PairResponse { accepted } => accepted,
                Msg::Reject { reason } => {
                    emit_done(handle, uid, &name, false, Some(&reason));
                    return Ok(());
                }
                other => return Err(anyhow!("se esperaba PairResponse, llego {other:?}")),
            };
            sess.send(&Msg::PairAck {
                accepted: accepted_here,
            })?;
            if !accepted_there {
                emit_done(handle, uid, &name, false, Some("rechazado en el otro dispositivo"));
                return Ok(());
            }
            store_device(handle, uid, &name, &platform_of(handle, uid), &sess.peer_pubkey)?;
            emit_done(handle, uid, &name, true, None);
            let _ = handle.emit("peers-changed", ());
            exchange_hello(handle, &mut sess, uid, &name)
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncPlanEvent {
    pub uid: String,
    pub name: String,
    pub plan: crate::manifest::Plan,
    pub bytes_in: i64,
    pub bytes_out: i64,
}

/// Simulacro de sync (Fase 5.3): pide el inventario del otro lado, lo compara
/// con el propio y publica lo que pasaría. **No escribe nada.**
pub fn preview_sync(handle: AppHandle, uid: String) {
    std::thread::spawn(move || {
        let name = peer_name(&handle, &uid);
        match preview_inner(&handle, &uid) {
            Ok(plan) => {
                if plan.is_empty() {
                    log::info!("[sync] {name}: nada que sincronizar");
                }
                let _ = handle.emit(
                    "sync-plan",
                    SyncPlanEvent {
                        uid: uid.clone(),
                        name,
                        bytes_in: plan.bytes_in(),
                        bytes_out: plan.bytes_out(),
                        plan,
                    },
                );
            }
            Err(e) => {
                log::warn!("[sync] preview con {uid} fallo: {e}");
                emit_done(&handle, &uid, &name, false, Some(&e.to_string()));
            }
        }
    });
}

/// Abre una sesión con un dispositivo ya vinculado y se presenta.
fn open_session(handle: &AppHandle, uid: &str) -> Result<Session> {
    let addr = peer_addr(handle, uid)?;
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let private = private_key(handle)?;
    let mut sess = Session::connect(stream, &private)?;

    match known_state(handle, uid, &sess.peer_pubkey) {
        Known::Trusted => {}
        Known::KeyMismatch => return Err(anyhow!("la clave del dispositivo no coincide")),
        Known::Unknown => return Err(anyhow!("todavía no está vinculado")),
    }

    let (my_uid, my_name) = me(handle)?;
    let (tracks, playlists) = library_counts(handle);
    sess.send(&Msg::Hello {
        uid: my_uid,
        name: my_name,
        platform: platform(),
        tracks,
        playlists,
        clock_ms: db::now_ms(),
    })?;
    match sess.recv()? {
        Msg::Hello { .. } => Ok(sess),
        Msg::NotPaired => {
            forget_device(handle, uid)?;
            let _ = handle.emit("peers-changed", ());
            Err(anyhow!("ese dispositivo ya no te tiene vinculado"))
        }
        other => Err(anyhow!("se esperaba Hello, llego {other:?}")),
    }
}

/// Pide el inventario del otro lado y calcula el plan. Devuelve también el
/// manifest remoto: la transferencia necesita la metadata de cada track para
/// dar de alta lo que reciba con el uid del otro dispositivo.
fn fetch_plan(
    handle: &AppHandle,
    sess: &mut Session,
) -> Result<(crate::manifest::Plan, crate::manifest::Manifest)> {
    sess.send(&Msg::ManifestReq)?;
    let remote = match sess.recv()? {
        Msg::ManifestData { manifest } => *manifest,
        other => return Err(anyhow!("se esperaba el manifest, llego {other:?}")),
    };
    let local = {
        let state = handle.state::<AppState>();
        let conn = state.db.lock().map_err(|_| anyhow!("db lock"))?;
        crate::manifest::build(&conn)?
    };
    Ok((crate::manifest::plan(&local, &remote), remote))
}

fn preview_inner(handle: &AppHandle, uid: &str) -> Result<crate::manifest::Plan> {
    let mut sess = open_session(handle, uid)?;
    let (plan, _) = fetch_plan(handle, &mut sess)?;
    let _ = sess.send(&Msg::Bye);
    Ok(plan)
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncProgressEvent {
    uid: String,
    /// Índice del archivo actual y total de archivos de esta corrida.
    file_index: usize,
    file_total: usize,
    filename: String,
    /// Bytes del archivo actual.
    done: u64,
    total: u64,
    sending: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncDoneEvent {
    uid: String,
    name: String,
    received: usize,
    sent: usize,
    failed: usize,
    bytes: u64,
    /// Registros de organización (metadata, playlists, membresías) aplicados
    /// entre los dos lados. Sin esto, un sync que sólo movió playlists
    /// reportaría "nada que transferir", que es falso.
    organized: usize,
    /// Lo disparó el sync automático, no el usuario. La UI no avisa de los
    /// automáticos que no hicieron nada: serían un cartel cada pocos minutos.
    auto: bool,
    error: Option<String>,
}

/// Ejecuta la transferencia de archivos del plan (Fase 5.4).
///
/// Sólo archivos: metadata, playlists y borrados son 5.5 y 5.6. Un archivo
/// que falla no corta la corrida — se cuenta y se sigue con el siguiente,
/// porque quedarse a mitad por un archivo ilegible sería peor que terminar
/// con un faltante.
pub fn sync_files(handle: AppHandle, uid: String) {
    run_sync(handle, uid, false)
}

/// Igual, pero disparado por el sync automático.
pub fn sync_files_auto(handle: AppHandle, uid: String) {
    run_sync(handle, uid, true)
}

fn run_sync(handle: AppHandle, uid: String, auto: bool) {
    std::thread::spawn(move || {
        let Some(_guard) = SyncGuard::acquire(&handle, &uid) else {
            log::debug!("[sync] ya hay un sync corriendo con {uid}");
            return;
        };
        let name = peer_name(&handle, &uid);
        match sync_inner(&handle, &uid) {
            Ok(SyncResult { received, sent, failed, bytes, organized }) => {
                log::info!(
                    "[sync] {name}: {received} recibidos, {sent} enviados, {failed} fallados, {organized} de organización"
                );
                let _ = handle.emit(
                    "sync-done",
                    SyncDoneEvent {
                        uid: uid.clone(),
                        name,
                        received,
                        sent,
                        failed,
                        bytes,
                        organized,
                        auto,
                        error: None,
                    },
                );
                let _ = handle.emit("library-changed", ());
            }
            Err(e) => {
                log::warn!("[sync] {name} fallo: {e}");
                let _ = handle.emit(
                    "sync-done",
                    SyncDoneEvent {
                        uid: uid.clone(),
                        name,
                        received: 0,
                        sent: 0,
                        failed: 0,
                        bytes: 0,
                        organized: 0,
                        auto,
                        error: Some(e.to_string()),
                    },
                );
            }
        }
    });
}

struct SyncResult {
    received: usize,
    sent: usize,
    failed: usize,
    bytes: u64,
    organized: usize,
}

fn sync_inner(handle: &AppHandle, uid: &str) -> Result<SyncResult> {
    let mut sess = open_session(handle, uid)?;
    let (plan, remote) = fetch_plan(handle, &mut sess)?;
    let music_dir = handle.state::<AppState>().music_dir.clone();

    let total_files = plan.pull_files.len() + plan.push_files.len();
    let (mut received, mut sent, mut failed, mut bytes) = (0usize, 0usize, 0usize, 0u64);
    let mut index = 0usize;

    // --- Traer lo que falta acá ------------------------------------------
    for f in &plan.pull_files {
        index += 1;
        let entry = remote.tracks.iter().find(|t| t.uid == f.track_uid);
        let Some(entry) = entry else { continue };
        let h = handle.clone();
        let uid_owned = uid.to_string();
        let fname = f.filename.clone();
        let mut progress = |done: u64, total: u64| {
            let _ = h.emit(
                "sync-progress",
                SyncProgressEvent {
                    uid: uid_owned.clone(),
                    file_index: index,
                    file_total: total_files,
                    filename: fname.clone(),
                    done,
                    total,
                    sending: false,
                },
            );
        };
        let h2 = handle.clone();
        let got = crate::transfer::pull_file(
            &mut sess,
            &music_dir,
            &f.hash,
            &f.filename,
            &mut progress,
            &mut |dest| h2.state::<AppState>().expect_path(dest),
        );
        match got {
            Ok(got) => {
                let state = handle.state::<AppState>();
                let conn = state.db.lock().map_err(|_| anyhow!("db lock"))?;
                crate::transfer::insert_received(
                    &conn,
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
                bytes += got.bytes;
                received += 1;
            }
            Err(e) => {
                log::warn!("[sync] no se pudo traer {}: {e}", f.filename);
                failed += 1;
            }
        }
    }

    // --- Mandar lo que falta allá ----------------------------------------
    for f in &plan.push_files {
        index += 1;
        let local = {
            let state = handle.state::<AppState>();
            let conn = state.db.lock().map_err(|_| anyhow!("db lock"))?;
            local_track(&conn, &f.track_uid)
        };
        let Some((path, entry)) = local else {
            failed += 1;
            continue;
        };
        let _ = handle.emit(
            "sync-progress",
            SyncProgressEvent {
                uid: uid.to_string(),
                file_index: index,
                file_total: total_files,
                filename: f.filename.clone(),
                done: 0,
                total: f.size as u64,
                sending: true,
            },
        );
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
        match push.and_then(|_| crate::transfer::send_file(&mut sess, &path, 0, &f.hash)) {
            Ok(()) => {
                bytes += f.size as u64;
                sent += 1;
            }
            Err(e) => {
                log::warn!("[sync] no se pudo mandar {}: {e}", f.filename);
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
    let local = {
        let state = handle.state::<AppState>();
        let conn = state.db.lock().map_err(|_| anyhow!("db lock"))?;
        crate::manifest::build(&conn)?
    };
    let mine = crate::merge::changes_for_peer(&local, &remote);
    let theirs = crate::merge::changes_for_peer(&remote, &local);

    let applied_here = {
        let state = handle.state::<AppState>();
        let conn = state.db.lock().map_err(|_| anyhow!("db lock"))?;
        let policy = delete_policy(&conn, uid);
        crate::merge::apply(&conn, &theirs, &music_dir, policy)?
    };
    sess.send(&Msg::MetaPush {
        changes: Box::new(mine),
    })?;
    let applied_there = match sess.recv()? {
        Msg::MetaAck { applied } => applied,
        other => return Err(anyhow!("se esperaba MetaAck, llegó {other:?}")),
    };
    // Desglosado y no sólo el total: si un sync repite los mismos números
    // corrida tras corrida, no está convergiendo, y el total solo no dice
    // qué se está re-aplicando.
    log::info!(
        "[sync] acá: {} meta, {} playlists, {} membresías, {} borrados | allá: {} meta, {} playlists, {} membresías, {} borrados",
        applied_here.tracks,
        applied_here.playlists,
        applied_here.memberships,
        applied_here.deleted,
        applied_there.tracks,
        applied_there.playlists,
        applied_there.memberships,
        applied_there.deleted
    );
    if applied_here.total() > 0 {
        log::debug!(
            "[sync] entrantes: {} tracks, {} playlists, {} membresías, {} tombstones",
            theirs.tracks.len(),
            theirs.playlists.len(),
            theirs.memberships.len(),
            theirs.tombstones.len()
        );
    }

    let _ = sess.send(&Msg::Bye);
    Ok(SyncResult {
        received,
        sent,
        failed,
        bytes,
        organized: applied_here.total() + applied_there.total(),
    })
}

/// Datos de un track local por uid: el path real y su entrada de manifest.
fn local_track(
    conn: &rusqlite::Connection,
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

/// Qué hacer con los borrados que manda ESTE peer. Se evalúa del lado que
/// recibe: poner `ask` en la PC principal la protege de un borrado hecho en
/// otro dispositivo sin bloquear el resto del sync.
fn delete_policy(conn: &rusqlite::Connection, peer_uid: &str) -> crate::merge::DeletePolicy {
    let s: String = conn
        .query_row(
            "SELECT deletes FROM sync_policy WHERE device_uid = ?1",
            [peer_uid],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "propagate".into());
    crate::merge::DeletePolicy::from_setting(&s)
}

fn platform_of(handle: &AppHandle, uid: &str) -> String {
    handle
        .state::<AppState>()
        .peers
        .list()
        .into_iter()
        .find(|p| p.uid == uid)
        .map(|p| p.platform)
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Comunes
// ---------------------------------------------------------------------------

/// Prueba de que el canal quedó vivo: cada lado le dice al otro cuánta
/// biblioteca tiene. Es lo primero que se ve funcionar de punta a punta sin
/// haber movido un solo byte de audio.
fn exchange_hello(handle: &AppHandle, sess: &mut Session, uid: &str, name: &str) -> Result<()> {
    let (my_uid, my_name) = me(handle)?;
    let (tracks, playlists) = library_counts(handle);
    sess.send(&Msg::Hello {
        uid: my_uid,
        name: my_name,
        platform: platform(),
        tracks,
        playlists,
        clock_ms: db::now_ms(),
    })?;
    match sess.recv()? {
        Msg::Hello {
            tracks,
            playlists,
            clock_ms,
            ..
        } => {
            report_hello(handle, uid, name, tracks, playlists, clock_ms);
            Ok(())
        }
        other => Err(anyhow!("se esperaba Hello, llego {other:?}")),
    }
}

fn report_hello(
    handle: &AppHandle,
    uid: &str,
    name: &str,
    tracks: i64,
    playlists: i64,
    their_clock: i64,
) {
    let skew = their_clock - db::now_ms();
    if skew.abs() > 5 * 60 * 1000 {
        log::warn!("[pair] reloj de {name} corrido {skew} ms — el merge por LWW puede elegir mal");
    }
    {
        let state = handle.state::<AppState>();
        // El guard va a una variable propia: como binding del `if let` sería
        // un temporario que vive más que `state`, y no compila.
        let db = state.db.lock();
        if let Ok(conn) = db {
            let _ = conn.execute(
                "UPDATE devices SET last_seen = ?1, name = ?2 WHERE uid = ?3",
                rusqlite::params![db::now_ms(), name, uid],
            );
        }
    }
    let _ = handle.emit(
        "peer-hello",
        PeerHelloEvent {
            uid: uid.to_string(),
            name: name.to_string(),
            tracks,
            playlists,
            clock_skew_ms: skew,
        },
    );
}

fn emit_done(handle: &AppHandle, uid: &str, name: &str, ok: bool, error: Option<&str>) {
    let _ = handle.emit(
        "pairing-done",
        PairingDoneEvent {
            uid: uid.to_string(),
            name: name.to_string(),
            ok,
            error: error.map(|s| s.to_string()),
        },
    );
}

/// Una clave distinta para un uid conocido puede ser una reinstalación del
/// otro lado — o alguien haciéndose pasar por él. No se resuelve solo: queda
/// registrado y el usuario tiene que desvincular a mano para volver a parear.
fn warn_key_mismatch(handle: &AppHandle, uid: &str, name: &str) {
    log::warn!("[pair] clave distinta para {name} ({uid}) — conexión rechazada");
    {
        let state = handle.state::<AppState>();
        // El guard va a una variable propia: como binding del `if let` sería
        // un temporario que vive más que `state`, y no compila.
        let db = state.db.lock();
        if let Ok(conn) = db {
            let _ = conn.execute(
                "INSERT INTO sync_log (ts, peer, kind, detail) VALUES (?1, ?2, 'key-mismatch', ?3)",
                rusqlite::params![db::now_ms(), uid, name],
            );
        }
    }
    emit_done(
        handle,
        uid,
        name,
        false,
        Some("la clave de este dispositivo cambió — desvinculalo y volvé a vincularlo si fuiste vos"),
    );
}

fn forget_device(handle: &AppHandle, uid: &str) -> Result<()> {
    let state = handle.state::<AppState>();
    let conn = state.db.lock().map_err(|_| anyhow!("db lock"))?;
    conn.execute("DELETE FROM devices WHERE uid = ?1", [uid])?;
    Ok(())
}

/// Saca un dispositivo de la lista de confiados y **le avisa**.
///
/// El pairing se guarda de los dos lados. Sin el aviso, desvincular acá dejaba
/// al otro mostrando "Paired" para siempre, y ningún Refresh lo iba a
/// corregir: la lista de dispositivos no tiene nada que ver con lo que ve
/// mDNS. El aviso es best-effort — si el otro está apagado, se entera solo la
/// próxima vez que intente conectarse y reciba `NotPaired`.
pub fn unpair(handle: &AppHandle, uid: &str) -> Result<()> {
    let addr = peer_addr(handle, uid).ok();
    forget_device(handle, uid)?;
    if let Some(addr) = addr {
        let handle = handle.clone();
        let uid = uid.to_string();
        std::thread::spawn(move || {
            if let Err(e) = notify_unpair(&handle, addr) {
                log::debug!("[pair] no se pudo avisar el unpair a {uid}: {e}");
            }
        });
    }
    Ok(())
}

fn notify_unpair(handle: &AppHandle, addr: SocketAddr) -> Result<()> {
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    stream.set_write_timeout(Some(CONNECT_TIMEOUT))?;
    let private = private_key(handle)?;
    let mut sess = Session::connect(stream, &private)?;
    let (my_uid, _) = me(handle)?;
    sess.send(&Msg::Unpair { uid: my_uid })
}
