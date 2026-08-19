//! What the server does with an incoming connection.
//!
//! It's the same ceremony the app runs (see `app/src-tauri/src/pairing.rs`)
//! with a single difference, and it's the one that justifies this file's
//! existence: when a `PairRequest` arrives, the app shows a person six digits
//! and waits; here there's no person, so the proof is the config's token.
//!
//! Everything else — verifying the key against the one already stored,
//! rejecting one that doesn't match, registering the device, serving the
//! manifest and the files — is literally `sway_core`'s code. A separate copy
//! of the protocol here would be a second implementation to keep up to date,
//! and the one that falls behind corrupts libraries.

use crate::host::ServerHost;
use anyhow::{anyhow, Result};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use sway_core::engine::{self, is_disconnect};
use sway_core::pairing::{self as pair, Known};
use sway_core::wire::{Msg, Session};
use sway_core::{db, engine::Host};

pub struct Server {
    pub host: Arc<ServerHost>,
    pub token: String,
}

pub fn run(server: Arc<Server>, listener: TcpListener) -> Result<()> {
    log::info!("[server] listening on {}", listener.local_addr()?);
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                log::warn!("[server] accept failed: {e}");
                continue;
            }
        };
        let server = Arc::clone(&server);
        std::thread::spawn(move || {
            let peer = stream
                .peer_addr()
                .map(|a| a.to_string())
                .unwrap_or_else(|_| "?".into());
            if let Err(e) = serve(&server, stream) {
                // Connecting and hanging up without saying anything is what
                // the app's reachability probe does: expected traffic, not
                // an error.
                if is_disconnect(&e) {
                    log::debug!("[server] {peer} disconnected without saying anything");
                } else {
                    log::warn!("[server] connection with {peer} ended: {e}");
                }
            }
        });
    }
    Ok(())
}

fn serve(server: &Server, stream: TcpStream) -> Result<()> {
    stream.set_read_timeout(Some(pair::IO_TIMEOUT))?;
    stream.set_write_timeout(Some(pair::IO_TIMEOUT))?;
    let private = server.host.with_db(|conn| Ok(pair::keypair(conn)?.0))?;
    let mut sess = Session::accept(stream, &private)?;

    match sess.recv()? {
        Msg::PairRequest {
            uid,
            name,
            platform,
            token,
        } => pair_device(server, &mut sess, &uid, &name, &platform, token.as_deref()),

        Msg::Hello {
            uid,
            name,
            tracks,
            playlists,
            clock_ms,
            ..
        } => {
            let known = server
                .host
                .with_db(|conn| Ok(pair::known_state(conn, &uid, &sess.peer_pubkey)))?;
            match known {
                Known::Trusted => {}
                Known::KeyMismatch => {
                    // Logged BEFORE answering: if the connection drops while
                    // sending the rejection, the attempt still has to be
                    // recorded. A security event doesn't depend on the other
                    // side sticking around to listen.
                    server
                        .host
                        .with_db(|conn| Ok(pair::log_key_mismatch(conn, &uid, &name)))?;
                    let _ = sess.send(&Msg::Reject {
                        reason: "different key from the one already stored for this device".into(),
                    });
                    return Err(anyhow!("different key for {uid}"));
                }
                Known::Unknown => {
                    // Let it find out and clean up its own row, instead of
                    // continuing to try against a server that doesn't have it.
                    let _ = sess.send(&Msg::NotPaired);
                    return Ok(());
                }
            }
            hello_back(server, &mut sess)?;
            report_clock(&name, clock_ms);
            server
                .host
                .with_db(|conn| Ok(pair::touch_device(conn, &uid, &name)))?;
            log::info!("[server] {name} connected ({tracks} tracks, {playlists} playlists)");
            let stats = engine::serve_requests(&*server.host, &mut sess, &uid)?;
            // Whoever handles this doesn't decide anything: it just answers
            // requests. Without this summary its log is a string of loose
            // lines with no way to tell at a glance whether the run moved
            // anything — and there's no screen here to check it another way.
            if stats.moved_something() {
                log::info!(
                    "[server] {name}: {} received, {} sent, {} organization, {} deleted",
                    stats.received,
                    stats.sent,
                    stats.applied.tracks + stats.applied.playlists + stats.applied.memberships,
                    stats.applied.deleted,
                );
            } else {
                log::info!("[server] {name}: already up to date");
            }
            Ok(())
        }

        // They removed it from the list on the other side. Only valid if its
        // key is the one we had stored — that is, if the handshake proved
        // it's really them.
        Msg::Unpair { uid } => {
            let known = server
                .host
                .with_db(|conn| Ok(pair::known_state(conn, &uid, &sess.peer_pubkey)))?;
            match known {
                Known::Trusted => {
                    server.host.with_db(|conn| pair::forget_device(conn, &uid))?;
                    log::info!("[server] {uid} unpaired");
                    Ok(())
                }
                _ => Err(anyhow!("unpair from a device that is not paired ({uid})")),
            }
        }

        other => Err(anyhow!("unexpected first message: {other:?}")),
    }
}

/// Pairing without a screen: the token does what the six digits do over there.
fn pair_device(
    server: &Server,
    sess: &mut Session,
    uid: &str,
    name: &str,
    platform: &str,
    token: Option<&str>,
) -> Result<()> {
    let known = server
        .host
        .with_db(|conn| Ok(pair::known_state(conn, uid, &sess.peer_pubkey)))?;
    if let Known::KeyMismatch = known {
        // Same as above: logged first, answered after.
        server
            .host
            .with_db(|conn| Ok(pair::log_key_mismatch(conn, uid, name)))?;
        let _ = sess.send(&Msg::Reject {
            reason: "different key from the one already stored for this device".into(),
        });
        return Err(anyhow!("different key for {uid}"));
    }

    // The token is compared in full (see `pair::secret_eq`): with a
    // comparison that short-circuits on the first mismatch, response time
    // leaks how many characters at the start are correct.
    let ok = token.map(|t| pair::secret_eq(t, &server.token)).unwrap_or(false);
    if !ok {
        let _ = sess.send(&Msg::PairResponse { accepted: false });
        log::warn!("[server] {name} ({uid}) tried to pair with a wrong token");
        return Err(anyhow!("invalid token"));
    }

    sess.send(&Msg::PairResponse { accepted: true })?;
    // The other side also has to accept: it's enough for just one of them to
    // say no for there to be no pairing.
    match sess.recv()? {
        Msg::PairAck { accepted: true } => {}
        Msg::PairAck { accepted: false } => {
            log::info!("[server] {name} cancelled pairing");
            return Ok(());
        }
        Msg::Reject { reason } => {
            log::info!("[server] {name} rejected pairing: {reason}");
            return Ok(());
        }
        other => return Err(anyhow!("expected PairAck, got {other:?}")),
    }

    server
        .host
        .with_db(|conn| pair::store_device(conn, uid, name, platform, &sess.peer_pubkey))?;
    log::info!("[server] {name} ({platform}) paired");

    // Mutual introduction, just like between two devices: both send first
    // and wait afterward, so neither gets stuck.
    hello_back(server, sess)?;
    match sess.recv()? {
        Msg::Hello { clock_ms, .. } => {
            report_clock(name, clock_ms);
            Ok(())
        }
        other => Err(anyhow!("expected Hello, got {other:?}")),
    }
}

fn hello_back(server: &Server, sess: &mut Session) -> Result<()> {
    let (uid, name, tracks, playlists) = server.host.with_db(|conn| {
        let (uid, name) = pair::me(conn)?;
        let (tracks, playlists) = pair::library_counts(conn);
        Ok((uid, name, tracks, playlists))
    })?;
    sess.send(&Msg::Hello {
        uid,
        name,
        platform: pair::PLATFORM_SERVER.into(),
        tracks,
        playlists,
        clock_ms: db::now_ms(),
    })
}

/// A clock that's off picks the wrong side in an LWW merge, and on a server
/// nobody watches that's exactly the kind of thing that isn't discovered
/// until it's already happened.
fn report_clock(name: &str, their_clock: i64) {
    let skew = their_clock - db::now_ms();
    if skew.abs() > 5 * 60 * 1000 {
        log::warn!("[server] clock of {name} is off by {skew} ms - last-write-wins may pick the wrong side");
    }
}
