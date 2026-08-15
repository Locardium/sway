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
use crate::engine::{self, is_disconnect};
use crate::wire::{Msg, Session};
use crate::AppState;
use anyhow::{anyhow, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{channel, Sender};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

/// Cada `library-changed` hace que el frontend recargue la biblioteca ENTERA
/// por IPC. Emitirlo por archivo recibido significa, en una corrida de 100
/// archivos, 100 recargas completas peleando por el mismo lock de SQLite que
/// está usando la transferencia — en el celular la app se traba y no se puede
/// ni abrir una pantalla. Se emite como mucho una vez cada esto; el final de
/// la corrida siempre emite.
const LIBRARY_EVENT_MIN_MS: i64 = 1500;
static LAST_LIBRARY_EVENT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

pub fn emit_library_changed(handle: &AppHandle, force: bool) {
    use std::sync::atomic::Ordering;
    let now = db::now_ms();
    if !force && now - LAST_LIBRARY_EVENT.load(Ordering::Relaxed) < LIBRARY_EVENT_MIN_MS {
        return;
    }
    LAST_LIBRARY_EVENT.store(now, Ordering::Relaxed);
    let _ = handle.emit("library-changed", ());
}

/// Cuánto se espera a que una persona mire la pantalla y confirme.
const DECISION_TIMEOUT: Duration = Duration::from_secs(120);

// Lo que no necesita pantalla vive en `sway_core::pairing` desde la Fase 6.1:
// claves, estado de `devices`, conteos y timeouts. Acá queda la ceremonia —
// mostrar el código y esperar a que alguien lo mire — y los eventos de ventana.
// Las funciones de abajo son la misma operación con el `AppHandle` puesto.
use sway_core::pairing as core_pair;
use sway_core::pairing::{platform, Known, CONNECT_TIMEOUT, IO_TIMEOUT};

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

fn private_key(handle: &AppHandle) -> Result<Vec<u8>> {
    let state = handle.state::<AppState>();
    let conn = state.db.lock().map_err(|_| anyhow!("db lock"))?;
    Ok(core_pair::keypair(&conn)?.0)
}

// ---------------------------------------------------------------------------
// Estado de `devices`
// ---------------------------------------------------------------------------

fn known_state(handle: &AppHandle, uid: &str, pubkey: &[u8]) -> Known {
    let state = handle.state::<AppState>();
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(_) => return Known::Unknown,
    };
    core_pair::known_state(&conn, uid, pubkey)
}

fn store_device(handle: &AppHandle, uid: &str, name: &str, platform: &str, pubkey: &[u8]) -> Result<()> {
    let state = handle.state::<AppState>();
    let conn = state.db.lock().map_err(|_| anyhow!("db lock"))?;
    core_pair::store_device(&conn, uid, name, platform, pubkey)
}

fn library_counts(handle: &AppHandle) -> (i64, i64) {
    let state = handle.state::<AppState>();
    // El guard va a una variable propia: como binding del `match` sería un
    // temporario que vive más que `state`, y no compila.
    let db = state.db.lock();
    match db {
        Ok(conn) => core_pair::library_counts(&conn),
        Err(_) => (0, 0),
    }
}

fn me(handle: &AppHandle) -> Result<(String, String)> {
    let state = handle.state::<AppState>();
    let conn = state.db.lock().map_err(|_| anyhow!("db lock"))?;
    core_pair::me(&conn)
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
        log::info!("[pair] listening on {:?}", listener.local_addr());
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("[pair] accept failed: {e}");
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
                        log::debug!("[pair] reachability probe");
                    } else {
                        log::warn!("[pair] incoming connection ended: {e}");
                    }
                }
            });
        }
    });
}

fn serve(handle: &AppHandle, stream: TcpStream) -> Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let private = private_key(handle)?;
    let mut sess = Session::accept(stream, &private)?;

    match sess.recv()? {
        // El token que trae un `PairRequest` es para los dispositivos sin
        // pantalla. Acá hay una: la prueba la da la persona comparando el
        // código, así que el token se ignora aunque venga.
        Msg::PairRequest { uid, name, platform, token: _ } => {
            match known_state(handle, &uid, &sess.peer_pubkey) {
                Known::KeyMismatch => {
                    let _ = sess.send(&Msg::Reject {
                        reason: "different key from the one you already had for this device".into(),
                    });
                    warn_key_mismatch(handle, &uid, &name);
                    return Err(anyhow!("different key for {uid}"));
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
                emit_done(handle, &uid, &name, false, Some("rejected on this device"));
                return Ok(());
            }
            // El otro lado también tiene que haber aceptado.
            let accepted_there = match sess.recv()? {
                Msg::PairAck { accepted } => accepted,
                Msg::Reject { reason } => {
                    emit_done(handle, &uid, &name, false, Some(&reason));
                    return Ok(());
                }
                other => return Err(anyhow!("expected PairAck, got {other:?}")),
            };
            if !accepted_there {
                emit_done(handle, &uid, &name, false, Some("rejected on the other device"));
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
                        reason: "different key from the one you already had for this device".into(),
                    });
                    warn_key_mismatch(handle, &uid, &name);
                    return Err(anyhow!("different key for {uid}"));
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
            // Los totales le sirven a quien no tiene pantalla (el server, ver
            // `crates/server/src/serve.rs`); acá lo que pasó ya se vio salir
            // por los eventos de progreso.
            engine::serve_requests(&crate::AppHost(handle), &mut sess, &uid).map(|_| ())
        }
        // El otro lado nos sacó de sus dispositivos. Solo se acepta si su
        // clave es la que teníamos guardada — o sea, si el handshake probó
        // que es realmente él y no alguien pidiendo que nos desvinculemos.
        Msg::Unpair { uid } => {
            match known_state(handle, &uid, &sess.peer_pubkey) {
                Known::Trusted => {
                    forget_device(handle, &uid)?;
                    log::info!("[pair] {uid} unpaired us");
                    let _ = handle.emit("peers-changed", ());
                    Ok(())
                }
                _ => Err(anyhow!("unpair from a device that is not paired ({uid})")),
            }
        }
        other => Err(anyhow!("unexpected first message: {other:?}")),
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
            log::warn!("[pair] connection with {uid} failed: {e}");
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
        .ok_or_else(|| anyhow!("that device is no longer visible on the network"))?;
    let addr = peer
        .addrs
        .first()
        .ok_or_else(|| anyhow!("that device did not publish any address"))?;
    crate::discovery::resolve(addr, peer.port)
        .ok_or_else(|| anyhow!("could not resolve the address ({addr}:{})", peer.port))
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
                "the key of {name} does not match the one you had stored"
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
                    Err(anyhow!("{name} no longer has you paired"))
                }
                Msg::Reject { reason } => Err(anyhow!(reason)),
                other => Err(anyhow!("expected Hello, got {other:?}")),
            }
        }
        Known::Unknown => {
            sess.send(&Msg::PairRequest {
                uid: my_uid,
                name: my_name,
                platform: platform(),
                // Vincularse con otro dispositivo con pantalla: el código de
                // seis dígitos alcanza. El token es para el server (Fase 6.3).
                token: None,
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
                    reason: "rejected on the other device".into(),
                });
                emit_done(handle, uid, &name, false, Some("rejected on this device"));
                return Ok(());
            }
            let accepted_there = match sess.recv()? {
                Msg::PairResponse { accepted } => accepted,
                Msg::Reject { reason } => {
                    emit_done(handle, uid, &name, false, Some(&reason));
                    return Ok(());
                }
                other => return Err(anyhow!("expected PairResponse, got {other:?}")),
            };
            sess.send(&Msg::PairAck {
                accepted: accepted_here,
            })?;
            if !accepted_there {
                emit_done(handle, uid, &name, false, Some("rejected on the other device"));
                return Ok(());
            }
            store_device(handle, uid, &name, &platform_of(handle, uid), &sess.peer_pubkey)?;
            emit_done(handle, uid, &name, true, None);
            let _ = handle.emit("peers-changed", ());
            exchange_hello(handle, &mut sess, uid, &name)
        }
    }
}

// ---------------------------------------------------------------------------
// Server de archivo (Fase 6.3)
// ---------------------------------------------------------------------------

/// Cuánto se espera al server. Los 180 s de `IO_TIMEOUT` son para el otro
/// caso, donde puede haber una persona decidiendo; acá contesta una máquina, y
/// esta llamada bloquea la pantalla mientras tanto.
const SERVER_IO_TIMEOUT: Duration = Duration::from_secs(15);

/// Vincula con un server de archivo, que no se descubre solo y no tiene
/// pantalla donde comparar un código.
///
/// Devuelve el nombre que declaró el server. A diferencia del pairing entre
/// dispositivos, esto NO vuelve enseguida: del otro lado no hay nadie que
/// tarde en decidir, así que la respuesta llega en el mismo viaje y la UI
/// puede mostrar el resultado sin esperar un evento.
pub fn pair_with_server(handle: &AppHandle, host: &str, port: u16, token: &str) -> Result<String> {
    let (host, port) = split_host_port(host, port);
    let host = host.as_str();
    let addr = crate::discovery::resolve(host, port)
        .ok_or_else(|| anyhow!("could not resolve {host}:{port}"))?;
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(SERVER_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(SERVER_IO_TIMEOUT))?;
    let private = private_key(handle)?;
    let mut sess = Session::connect(stream, &private)?;

    let (my_uid, my_name) = me(handle)?;
    sess.send(&Msg::PairRequest {
        uid: my_uid,
        name: my_name,
        platform: platform(),
        token: Some(token.to_string()),
    })?;
    match sess.recv()? {
        Msg::PairResponse { accepted: true } => {}
        Msg::PairResponse { accepted: false } => {
            return Err(anyhow!("the server rejected the token"))
        }
        Msg::Reject { reason } => return Err(anyhow!(reason)),
        other => return Err(anyhow!("expected PairResponse, got {other:?}")),
    }
    sess.send(&Msg::PairAck { accepted: true })?;

    // Recién acá se sabe con qué uid se está hablando: a un server se lo llama
    // por dirección, no por identidad. Por eso la comprobación de clave viene
    // después y no antes — pero viene, y antes de guardar nada.
    let (their_uid, their_name, their_platform) = match sess.recv()? {
        Msg::Hello {
            uid, name, platform, ..
        } => (uid, name, platform),
        Msg::Reject { reason } => return Err(anyhow!(reason)),
        other => return Err(anyhow!("expected Hello, got {other:?}")),
    };
    if let Known::KeyMismatch = known_state(handle, &their_uid, &sess.peer_pubkey) {
        warn_key_mismatch(handle, &their_uid, &their_name);
        return Err(anyhow!(
            "the key of {their_name} does not match the one you had stored"
        ));
    }

    let (tracks, playlists) = library_counts(handle);
    let (my_uid, my_name) = me(handle)?;
    sess.send(&Msg::Hello {
        uid: my_uid,
        name: my_name,
        platform: platform(),
        tracks,
        playlists,
        clock_ms: db::now_ms(),
    })?;

    store_device(handle, &their_uid, &their_name, &their_platform, &sess.peer_pubkey)?;
    {
        let state = handle.state::<AppState>();
        let conn = state.db.lock().map_err(|_| anyhow!("db lock"))?;
        core_pair::set_device_address(&conn, &their_uid, &format!("{host}:{port}"))?;
    }
    // Entra a la misma lista que los peers de mDNS: de acá para abajo es un
    // dispositivo más.
    handle
        .state::<AppState>()
        .peers
        .add_manual(&their_uid, &their_name, &their_platform, host, port);
    let _ = handle.emit("peers-changed", ());
    log::info!("[pair] server {their_name} paired at {host}:{port}");
    Ok(their_name)
}

/// Vuelve a poner en la lista los dispositivos con dirección fija. Los de la
/// LAN los repone mDNS solo; a estos no los anuncia nadie, así que sin esto
/// desaparecen en cada arranque.
pub fn restore_manual_peers(handle: &AppHandle) {
    let state = handle.state::<AppState>();
    let Ok(conn) = state.db.lock() else { return };
    for (uid, name, platform, address) in core_pair::devices_with_address(&conn) {
        let Some((host, port)) = split_addr(&address) else {
            log::warn!("[pair] invalid stored address for {name}: {address}");
            continue;
        };
        state.peers.add_manual(&uid, &name, &platform, host, port);
    }
}

/// Lo que la persona escribió, convertido en algo que se pueda resolver.
///
/// El campo pide un nombre o una IP, pero lo que uno tiene a mano es la URL del
/// server, y pegarla es lo que cualquiera haría. Sin esto, `https://casa.com/`
/// se pasaba tal cual a la resolución de nombres y fallaba con un
/// "could not resolve" que parecía un problema de DNS.
///
/// Si el texto trae su propio puerto, ese gana: es más específico que el del
/// campo de al lado, y es el que la persona acaba de leer en algún lado.
fn split_host_port(input: &str, default_port: u16) -> (String, u16) {
    let mut s = input.trim();
    // Esquema: `https://`, `http://`, `sway://`, lo que sea.
    if let Some((_, rest)) = s.split_once("://") {
        s = rest;
    }
    // Credenciales, por si vienen pegadas de una URL.
    if let Some((_, rest)) = s.rsplit_once('@') {
        s = rest;
    }
    // Ruta, query o fragmento: nada de eso es parte del host.
    s = s.split(['/', '?', '#']).next().unwrap_or(s);

    // IPv6 entre corchetes: `[fd00::1]:7420`.
    if let Some(rest) = s.strip_prefix('[') {
        if let Some((addr, tail)) = rest.split_once(']') {
            let port = tail.strip_prefix(':').and_then(|p| p.parse().ok());
            return (addr.to_string(), port.unwrap_or(default_port));
        }
    }
    // Un solo `:` es host:puerto. Varios son un IPv6 escrito sin corchetes, y
    // ahí no hay puerto que separar.
    if s.matches(':').count() == 1 {
        if let Some((h, p)) = s.split_once(':') {
            // Un puerto que no es número es un typo. Se queda el host y se usa
            // el del campo: vale más intentar que fallar con un "no se pudo
            // resolver casa.ejemplo:puerto", que manda a mirar el DNS.
            return (h.to_string(), p.parse().unwrap_or(default_port));
        }
    }
    (s.to_string(), default_port)
}

/// `host:puerto`, con el host pudiendo ser un nombre o un IPv6 entre corchetes.
fn split_addr(address: &str) -> Option<(&str, u16)> {
    let (host, port) = address.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    Some((host, port.parse().ok()?))
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
                    log::info!("[sync] {name}: nothing to sync");
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
                log::warn!("[sync] preview with {uid} failed: {e}");
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
        Known::KeyMismatch => return Err(anyhow!("the device key does not match")),
        Known::Unknown => return Err(anyhow!("not paired yet")),
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
            Err(anyhow!("that device no longer has you paired"))
        }
        other => Err(anyhow!("expected Hello, got {other:?}")),
    }
}

fn preview_inner(handle: &AppHandle, uid: &str) -> Result<crate::manifest::Plan> {
    let mut sess = open_session(handle, uid)?;
    let (mut plan, _, dir) = engine::fetch_plan(&crate::AppHost(handle), &mut sess)?;
    engine::restrict(&mut plan, dir);
    let _ = sess.send(&Msg::Bye);
    Ok(plan)
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

/// Espera a que terminen los syncs con esos dispositivos.
///
/// Es lo que deja poner al server siempre después de la red local: la LAN
/// mueve los archivos rápido y sin internet, y cuando después le toca al
/// server, el inventario ya cuenta lo que acaba de llegar — así que le pide
/// sólo lo que de verdad falta y nada viaja dos veces.
///
/// El respiro del principio no es de más: `sync_files_auto` toma su marca
/// recién adentro del hilo que lanza, así que preguntar en el acto encuentra
/// todo quieto y la espera no espera nada.
pub fn wait_until_idle(handle: &AppHandle, uids: &[String], max: Duration) {
    const GRACE: Duration = Duration::from_millis(800);
    const POLL: Duration = Duration::from_millis(250);
    std::thread::sleep(GRACE);
    let until = std::time::Instant::now() + max;
    while std::time::Instant::now() < until {
        let busy = {
            let state = handle.state::<AppState>();
            let active = state.pairing.active.lock();
            match active {
                Ok(a) => uids.iter().any(|u| a.contains(u)),
                Err(_) => false,
            }
        };
        if !busy {
            return;
        }
        std::thread::sleep(POLL);
    }
    log::warn!("[autosync] local network is still busy: syncing with the server anyway");
}

fn run_sync(handle: AppHandle, uid: String, auto: bool) {
    std::thread::spawn(move || {
        let Some(_guard) = SyncGuard::acquire(&handle, &uid) else {
            log::debug!("[sync] a sync with {uid} is already running");
            return;
        };
        let name = peer_name(&handle, &uid);
        match sync_inner(&handle, &uid) {
            Ok(engine::SyncResult { received, sent, failed, bytes, organized }) => {
                log::info!(
                    "[sync] {name}: {received} received, {sent} sent, {failed} failed, {organized} organization"
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
                // Fin de la corrida: acá sí, siempre.
                emit_library_changed(&handle, true);
            }
            Err(e) => {
                log::warn!("[sync] {name} failed: {e}");
                // Que se corte la conexión no es una falla que valga un cartel:
                // el celular se durmió, cambió de red, o se cerró la app del
                // otro lado. El sync automático lo reintenta solo. Se avisa
                // igual cuando el sync lo pidió una persona, que está mirando.
                let cut = is_disconnect(&e);
                let error = if cut {
                    if auto {
                        None
                    } else {
                        Some("connection lost".to_string())
                    }
                } else {
                    Some(e.to_string())
                };
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
                        error,
                    },
                );
            }
        }
    });
}

/// Una corrida completa contra un dispositivo ya vinculado.
///
/// Abrir la sesion es lo unico que sigue siendo asunto de la app (direccion,
/// claves, dispositivos conocidos); de ahi para adentro trabaja el motor, que
/// es el mismo codigo que ejercita la suite de integridad (ver engine.rs).
fn sync_inner(handle: &AppHandle, uid: &str) -> Result<engine::SyncResult> {
    let mut sess = open_session(handle, uid)?;
    engine::sync(&crate::AppHost(handle), &mut sess, uid)
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
        other => Err(anyhow!("expected Hello, got {other:?}")),
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
        log::warn!("[pair] clock of {name} is off by {skew} ms - last-write-wins may pick the wrong side");
    }
    {
        let state = handle.state::<AppState>();
        // El guard va a una variable propia: como binding del `if let` sería
        // un temporario que vive más que `state`, y no compila.
        let db = state.db.lock();
        if let Ok(conn) = db {
            core_pair::touch_device(&conn, uid, name);
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
    {
        let state = handle.state::<AppState>();
        // El guard va a una variable propia: como binding del `if let` sería
        // un temporario que vive más que `state`, y no compila.
        let db = state.db.lock();
        if let Ok(conn) = db {
            core_pair::log_key_mismatch(&conn, uid, name);
        }
    }
    emit_done(
        handle,
        uid,
        name,
        false,
        Some("this device's key changed - unlink it and pair again if that was you"),
    );
}

fn forget_device(handle: &AppHandle, uid: &str) -> Result<()> {
    let state = handle.state::<AppState>();
    let conn = state.db.lock().map_err(|_| anyhow!("db lock"))?;
    core_pair::forget_device(&conn, uid)
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
                log::debug!("[pair] could not notify unpair to {uid}: {e}");
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

#[cfg(test)]
mod tests {
    use super::{split_addr, split_host_port};

    #[test]
    fn pegar_la_url_del_server_funciona() {
        // Es lo que uno tiene a mano y lo que cualquiera va a pegar.
        assert_eq!(
            split_host_port("https://sway.ejemplo.com/", 7420),
            ("sway.ejemplo.com".into(), 7420)
        );
        assert_eq!(
            split_host_port("http://sway.ejemplo.com:9000/algo?x=1", 7420),
            ("sway.ejemplo.com".into(), 9000)
        );
        assert_eq!(
            split_host_port("  sway.ejemplo.com  ", 7420),
            ("sway.ejemplo.com".into(), 7420)
        );
    }

    #[test]
    fn el_puerto_escrito_le_gana_al_del_campo() {
        // Más específico, y es el que la persona acaba de leer en algún lado.
        assert_eq!(
            split_host_port("192.168.0.10:9999", 7420),
            ("192.168.0.10".into(), 9999)
        );
    }

    #[test]
    fn un_ipv6_no_se_confunde_con_un_puerto() {
        // Los `:` de un IPv6 no separan nada: sin corchetes no hay puerto.
        assert_eq!(split_host_port("fd00::1", 7420), ("fd00::1".into(), 7420));
        assert_eq!(
            split_host_port("[fd00::1]:9000", 7420),
            ("fd00::1".into(), 9000)
        );
        assert_eq!(split_host_port("[fd00::1]", 7420), ("fd00::1".into(), 7420));
    }

    #[test]
    fn un_puerto_que_no_es_numero_no_se_lleva_puesto_el_host() {
        // Vale más intentar con el puerto del campo que fallar por un typo.
        assert_eq!(
            split_host_port("casa.ejemplo:puerto", 7420),
            ("casa.ejemplo".into(), 7420)
        );
    }

    #[test]
    fn la_direccion_guardada_se_parte_en_host_y_puerto() {
        assert_eq!(split_addr("casa.ejemplo:7420"), Some(("casa.ejemplo", 7420)));
        assert_eq!(split_addr("192.168.0.10:7420"), Some(("192.168.0.10", 7420)));
        // IPv6: el host va entre corchetes justamente porque adentro hay `:`.
        assert_eq!(split_addr("[fd00::1]:7420"), Some(("fd00::1", 7420)));
    }

    #[test]
    fn una_direccion_sin_puerto_no_se_inventa_uno() {
        // Vale más quedarse sin el peer que sondear un puerto que nadie eligió.
        assert_eq!(split_addr("casa.ejemplo"), None);
        assert_eq!(split_addr("casa.ejemplo:"), None);
        assert_eq!(split_addr("casa.ejemplo:puerto"), None);
        assert_eq!(split_addr(""), None);
    }
}
