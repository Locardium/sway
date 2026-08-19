//! Device pairing (Phase 5.2).
//!
//! The Noise handshake leaves an encrypted channel with *someone*; pairing
//! is what turns it into an encrypted channel with *this specific device*.
//! Both sides show the same 6-digit code and the user confirms on both
//! screens — only then does the other side's public key get fixed in
//! `devices`.
//!
//! Hard rules:
//! - Pairing needs **both** sides to accept. Just one side seeing a
//!   different code is enough to abort.
//! - A different key for an already-known uid **gets rejected and flagged**.
//!   Trust is never silently re-established: that's exactly what a
//!   man-in-the-middle would do to impersonate one of your devices.
//! - An unpaired device receives no library data at all. The `Hello` with
//!   the counts comes after pairing, never before.

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

/// Every `library-changed` makes the frontend reload the ENTIRE library over
/// IPC. Emitting it per received file would mean, in a run of 100 files, 100
/// full reloads fighting for the same SQLite lock the transfer is using — on
/// the phone the app freezes and not even a screen can be opened. It's
/// emitted at most once per this interval; the end of the run always emits.
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

/// How long to wait for a person to look at the screen and confirm.
const DECISION_TIMEOUT: Duration = Duration::from_secs(120);

// Whatever doesn't need a screen lives in `sway_core::pairing` since Phase
// 6.1: keys, `devices` state, counts, and timeouts. What's left here is the
// ceremony — showing the code and waiting for someone to look at it — and
// the window events. The functions below are the same operation with the
// `AppHandle` plugged in.
use sway_core::pairing as core_pair;
use sway_core::pairing::{platform, Known, CONNECT_TIMEOUT, IO_TIMEOUT};

/// Pending pairing decisions, keyed by peer uid. The connection thread waits
/// on the receiver; the `confirm_pairing` command sends the answer.
#[derive(Default)]
pub struct Pairing {
    pending: Mutex<HashMap<String, Sender<bool>>>,
    /// Syncs in progress, by uid. With automatic sync there are several
    /// triggers (local change, peer showing up, periodic) that can fire
    /// almost together: two simultaneous runs against the same device
    /// would step on each other's half-downloaded files.
    active: Mutex<HashSet<String>>,
}

/// Marks a sync in progress; unmarks it on drop no matter what.
struct SyncGuard(AppHandle, String);

impl SyncGuard {
    fn acquire(handle: &AppHandle, uid: &str) -> Option<Self> {
        let state = handle.state::<AppState>();
        let mut active = state.pairing.active.lock().ok()?;
        if !active.insert(uid.to_string()) {
            return None; // one is already running with this peer
        }
        Some(SyncGuard(handle.clone(), uid.to_string()))
    }
}

impl Drop for SyncGuard {
    fn drop(&mut self) {
        let state = self.0.state::<AppState>();
        // The guard goes into its own variable: as an `if let` binding it
        // would be a temporary that outlives `state`, and it wouldn't compile.
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
    /// `true` if the other device initiated the pairing.
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
    /// Clock difference with the other device, in ms. Matters for the LWW
    /// merge from 5.5: with clocks off, "last write wins" picks wrong.
    pub clock_skew_ms: i64,
}

// ---------------------------------------------------------------------------
// This device's cryptographic identity
// ---------------------------------------------------------------------------

fn private_key(handle: &AppHandle) -> Result<Vec<u8>> {
    let state = handle.state::<AppState>();
    let conn = state.db.lock().map_err(|_| anyhow!("db lock"))?;
    Ok(core_pair::keypair(&conn)?.0)
}

// ---------------------------------------------------------------------------
// `devices` state
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
    // The guard goes into its own variable: as a `match` binding it would be
    // a temporary that outlives `state`, and it wouldn't compile.
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
// User confirmation
// ---------------------------------------------------------------------------

/// Shows the code and waits for the person to decide. The timeout prevents a
/// thread (and a connection) from being left hanging if nobody looks at the
/// screen.
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

/// Called by the `confirm_pairing` command from the UI.
pub fn resolve_decision(handle: &AppHandle, uid: &str, accepted: bool) -> bool {
    handle.state::<AppState>().pairing.resolve(uid, accepted)
}

// ---------------------------------------------------------------------------
// Side that accepts connections
// ---------------------------------------------------------------------------

/// Listens on the port 5.1 reserved and announced via mDNS.
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
                    // Reachability probes (see discovery::spawn_prober)
                    // connect and disconnect without sending anything: it's
                    // expected traffic, not an error worth reporting every
                    // 10 seconds.
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
        // The token a `PairRequest` carries is for devices without a screen.
        // There's one here: the proof comes from the person comparing the
        // code, so the token is ignored even if it's present.
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
            // The other side also has to have accepted.
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
                    // Not an error on the other side: we probably unpaired
                    // it. Let it know so it can clear its row.
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
            // The totals are useful to whoever has no screen (the server,
            // see `crates/server/src/serve.rs`); here what happened has
            // already gone out through the progress events.
            engine::serve_requests(&crate::AppHost(handle), &mut sess, &uid).map(|_| ())
        }
        // The other side removed us from its devices. Only accepted if its
        // key is the one we had stored — i.e. if the handshake proved it's
        // really them and not someone asking to unpair us.
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
// Calling side
// ---------------------------------------------------------------------------

/// Connects to a peer: pairs it if needed, and if already paired exchanges
/// `Hello`. Runs on its own thread — the other side might have a person
/// taking a while to confirm.
pub fn connect_peer(handle: AppHandle, uid: String) {
    std::thread::spawn(move || {
        let name = peer_name(&handle, &uid);
        if let Err(e) = connect_inner(&handle, &uid) {
            log::warn!("[pair] connection with {uid} failed: {e}");
            // Only NETWORK failures grey out the row: saying "connected"
            // right after a timeout is the worst possible combination. A
            // logical rejection (not paired, different key) doesn't mean the
            // device isn't there — greying it out would be lying too, just
            // in the other direction.
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
                // We got unpaired from the other side. It's credible: the
                // handshake already proved the key is the one we had
                // stored. Still showing "Paired" would be a lie.
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
                // Pairing with another device that has a screen: the
                // six-digit code is enough. The token is for the server
                // (Phase 6.3).
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
                // Cut it off here without waiting for the other side's
                // answer: there could be someone looking at the screen until
                // the timeout expires.
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
// File server (Phase 6.3)
// ---------------------------------------------------------------------------

/// How long to wait for the server. `IO_TIMEOUT`'s 180 s are for the other
/// case, where a person might be deciding; here a machine answers, and this
/// call blocks the screen in the meantime.
const SERVER_IO_TIMEOUT: Duration = Duration::from_secs(15);

/// Pairs with a file server, which isn't discovered on its own and has no
/// screen to compare a code on.
///
/// Returns the name the server declared. Unlike pairing between devices,
/// this does NOT return right away: there's no one on the other side who
/// might take a while to decide, so the response arrives on the same trip
/// and the UI can show the result without waiting for an event.
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

    // Only here do we learn which uid we're talking to: a server is called
    // by address, not by identity. That's why the key check comes after and
    // not before — but it does come, and before anything is stored.
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
    // Enters the same list as mDNS peers: from here down it's just another
    // device.
    handle
        .state::<AppState>()
        .peers
        .add_manual(&their_uid, &their_name, &their_platform, host, port);
    let _ = handle.emit("peers-changed", ());
    log::info!("[pair] server {their_name} paired at {host}:{port}");
    Ok(their_name)
}

/// Puts the fixed-address devices back on the list. The LAN ones are
/// restored by mDNS on its own; nobody announces these, so without this they
/// disappear on every startup.
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

/// What the person typed, turned into something resolvable.
///
/// The field asks for a name or an IP, but what people have at hand is the
/// server's URL, and pasting it is what anyone would do. Without this,
/// `https://home.com/` would get passed as-is to name resolution and fail
/// with a "could not resolve" that looked like a DNS problem.
///
/// If the text carries its own port, that one wins: it's more specific than
/// the one in the field next to it, and it's the one the person just read
/// somewhere.
fn split_host_port(input: &str, default_port: u16) -> (String, u16) {
    let mut s = input.trim();
    // Scheme: `https://`, `http://`, `sway://`, whatever.
    if let Some((_, rest)) = s.split_once("://") {
        s = rest;
    }
    // Credentials, in case they came pasted from a URL.
    if let Some((_, rest)) = s.rsplit_once('@') {
        s = rest;
    }
    // Path, query, or fragment: none of that is part of the host.
    s = s.split(['/', '?', '#']).next().unwrap_or(s);

    // IPv6 in brackets: `[fd00::1]:7420`.
    if let Some(rest) = s.strip_prefix('[') {
        if let Some((addr, tail)) = rest.split_once(']') {
            let port = tail.strip_prefix(':').and_then(|p| p.parse().ok());
            return (addr.to_string(), port.unwrap_or(default_port));
        }
    }
    // A single `:` is host:port. Several are an IPv6 written without
    // brackets, and there's no port to split off there.
    if s.matches(':').count() == 1 {
        if let Some((h, p)) = s.split_once(':') {
            // A port that isn't a number is a typo. The host is kept and the
            // one from the field is used: better to try than to fail with a
            // "could not resolve home.example:port", which sends people off
            // to check DNS.
            return (h.to_string(), p.parse().unwrap_or(default_port));
        }
    }
    (s.to_string(), default_port)
}

/// `host:port`, with the host possibly being a name or an IPv6 in brackets.
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

/// Sync dry run (Phase 5.3): asks the other side for its inventory, compares
/// it with its own, and publishes what would happen. **Writes nothing.**
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

/// Opens a session with an already paired device and introduces itself.
fn open_session(handle: &AppHandle, uid: &str) -> Result<Session> {
    open_session_with(handle, uid, IO_TIMEOUT)
}

/// Same, with the read timeout chosen by the caller.
///
/// Needed by the connection that waits for updates (`watch.rs`): there,
/// silence is normal, not a symptom, and `IO_TIMEOUT` would cut it off
/// between two heartbeats.
pub(crate) fn open_session_with(
    handle: &AppHandle,
    uid: &str,
    read_timeout: Duration,
) -> Result<Session> {
    let addr = peer_addr(handle, uid)?;
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(read_timeout))?;
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
    /// Organization records (metadata, playlists, memberships) applied
    /// between the two sides. Without this, a sync that only moved
    /// playlists would report "nothing to transfer", which is false.
    organized: usize,
    /// Triggered by automatic sync, not the user. The UI doesn't notify for
    /// automatic runs that did nothing: that would be a banner every few
    /// minutes.
    auto: bool,
    error: Option<String>,
}

/// Runs the file transfer for the plan (Phase 5.4).
///
/// Files only: metadata, playlists, and deletions are 5.5 and 5.6. A file
/// that fails doesn't stop the run — it's counted and the run continues with
/// the next one, because stopping halfway over one unreadable file would be
/// worse than finishing with something missing.
pub fn sync_files(handle: AppHandle, uid: String) {
    run_sync(handle, uid, false)
}

/// Same, but triggered by automatic sync.
pub fn sync_files_auto(handle: AppHandle, uid: String) {
    run_sync(handle, uid, true)
}

/// Waits for syncs with those devices to finish.
///
/// This is what always keeps the server after the local network: the LAN
/// moves files fast and without internet, and when it's later the server's
/// turn, the inventory already counts what just arrived — so it only asks
/// for what's really missing and nothing travels twice.
///
/// The initial breathing room isn't wasted: `sync_files_auto` only takes its
/// mark inside the thread it launches, so asking right away would find
/// everything quiet and the wait would wait for nothing.
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
    std::thread::spawn(move || run_sync_blocking(handle, uid, auto));
}

/// Same, on the caller's own thread.
///
/// Needed by the watcher (`watch.rs`): it has to catch up **before** it
/// starts waiting for updates, and if it doesn't wait for the run to finish
/// it would end up waiting against a state it's about to change itself.
pub(crate) fn run_sync_blocking(handle: AppHandle, uid: String, auto: bool) {
    {
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
                // End of the run: yes, always here.
                emit_library_changed(&handle, true);
            }
            Err(e) => {
                log::warn!("[sync] {name} failed: {e}");
                // A dropped connection isn't a failure worth a banner: the
                // phone went to sleep, switched networks, or the app closed
                // on the other side. Automatic sync retries on its own. It's
                // still reported when a person requested the sync and is
                // watching.
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
    }
}

/// A full run against an already paired device.
///
/// Opening the session is the only part still handled by the app (address,
/// keys, known devices); from there on the engine takes over, the same code
/// exercised by the integrity test suite (see engine.rs).
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
// Shared
// ---------------------------------------------------------------------------

/// Proof that the channel stayed alive: each side tells the other how much
/// library it has. It's the first thing seen working end to end without
/// having moved a single byte of audio.
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
        // The guard goes into its own variable: as an `if let` binding it
        // would be a temporary that outlives `state`, and it wouldn't compile.
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

/// A different key for a known uid could be a reinstall on the other side —
/// or someone impersonating it. It doesn't resolve itself: it gets logged
/// and the user has to unpair by hand to pair again.
fn warn_key_mismatch(handle: &AppHandle, uid: &str, name: &str) {
    {
        let state = handle.state::<AppState>();
        // The guard goes into its own variable: as an `if let` binding it
        // would be a temporary that outlives `state`, and it wouldn't compile.
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

/// Removes a device from the trusted list and **notifies it**.
///
/// Pairing is stored on both sides. Without the notification, unpairing here
/// would leave the other side showing "Paired" forever, and no Refresh was
/// going to fix it: the device list has nothing to do with what mDNS sees.
/// The notification is best-effort — if the other side is off, it finds out
/// only the next time it tries to connect and gets `NotPaired`.
pub fn unpair(handle: &AppHandle, uid: &str) -> Result<()> {
    let addr = peer_addr(handle, uid).ok();
    // If it had a fixed address (a server), it also leaves the on-screen
    // list. A LAN peer gets re-announced by mDNS within seconds; nobody
    // announces this one, so it would stay there forever with a "Pair"
    // button that can't work — that button drives the local-network flow,
    // with no token, and a server with no token says no.
    let manual = {
        let state = handle.state::<AppState>();
        let db = state.db.lock();
        match db {
            Ok(conn) => core_pair::devices_with_address(&conn)
                .iter()
                .any(|(u, _, _, _)| u == uid),
            Err(_) => false,
        }
    };
    forget_device(handle, uid)?;
    if manual {
        handle.state::<AppState>().peers.forget(uid);
    }
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
    fn pasting_the_server_url_works() {
        // It's what people have at hand and what anyone would paste.
        assert_eq!(
            split_host_port("https://sway.example.com/", 7420),
            ("sway.example.com".into(), 7420)
        );
        assert_eq!(
            split_host_port("http://sway.example.com:9000/something?x=1", 7420),
            ("sway.example.com".into(), 9000)
        );
        assert_eq!(
            split_host_port("  sway.example.com  ", 7420),
            ("sway.example.com".into(), 7420)
        );
    }

    #[test]
    fn the_written_port_wins_over_the_field() {
        // More specific, and it's the one the person just read somewhere.
        assert_eq!(
            split_host_port("192.168.0.10:9999", 7420),
            ("192.168.0.10".into(), 9999)
        );
    }

    #[test]
    fn an_ipv6_is_not_confused_with_a_port() {
        // The `:` in an IPv6 don't separate anything: no brackets, no port.
        assert_eq!(split_host_port("fd00::1", 7420), ("fd00::1".into(), 7420));
        assert_eq!(
            split_host_port("[fd00::1]:9000", 7420),
            ("fd00::1".into(), 9000)
        );
        assert_eq!(split_host_port("[fd00::1]", 7420), ("fd00::1".into(), 7420));
    }

    #[test]
    fn a_non_numeric_port_does_not_take_the_host_down_with_it() {
        // Better to try with the field's port than to fail over a typo.
        assert_eq!(
            split_host_port("home.example:port", 7420),
            ("home.example".into(), 7420)
        );
    }

    #[test]
    fn the_stored_address_splits_into_host_and_port() {
        assert_eq!(split_addr("home.example:7420"), Some(("home.example", 7420)));
        assert_eq!(split_addr("192.168.0.10:7420"), Some(("192.168.0.10", 7420)));
        // IPv6: the host goes in brackets precisely because it contains `:`.
        assert_eq!(split_addr("[fd00::1]:7420"), Some(("fd00::1", 7420)));
    }

    #[test]
    fn an_address_without_a_port_does_not_invent_one() {
        // Better to end up without the peer than to probe a port nobody chose.
        assert_eq!(split_addr("home.example"), None);
        assert_eq!(split_addr("home.example:"), None);
        assert_eq!(split_addr("home.example:port"), None);
        assert_eq!(split_addr(""), None);
    }
}
