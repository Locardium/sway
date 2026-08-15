//! Lo que hace el server con una conexión entrante.
//!
//! Es la misma ceremonia que corre la app (ver `app/src-tauri/src/pairing.rs`)
//! con una sola diferencia, y es la que justifica que exista este archivo:
//! cuando llega un `PairRequest`, la app le muestra seis dígitos a una persona
//! y espera; acá no hay persona, así que la prueba es el token de la config.
//!
//! Todo lo demás —verificar la clave contra la que ya teníamos, rechazar la
//! que no coincide, dar de alta el dispositivo, servir el manifiesto y los
//! archivos— es literalmente el código de `sway_core`. Una copia propia del
//! protocolo acá sería una segunda implementación para mantener al día, y la
//! que se quede atrás corrompe bibliotecas.

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
                // Conectar y cortar sin decir nada es lo que hace el sondeo de
                // alcanzabilidad de la app: tráfico esperado, no un error.
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
                    let _ = sess.send(&Msg::Reject {
                        reason: "different key from the one already stored for this device".into(),
                    });
                    server
                        .host
                        .with_db(|conn| Ok(pair::log_key_mismatch(conn, &uid, &name)))?;
                    return Err(anyhow!("different key for {uid}"));
                }
                Known::Unknown => {
                    // Que se entere y limpie su fila, en vez de seguir
                    // intentando contra un server que no lo tiene.
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
            // El que atiende no decide nada: responde pedidos. Sin este
            // resumen su log es una tira de líneas sueltas y no hay forma de
            // leer de un vistazo si la corrida movió algo — y acá no hay
            // ninguna pantalla donde mirarlo de otra manera.
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

        // Lo sacaron de la lista del otro lado. Sólo vale si su clave es la que
        // teníamos guardada — o sea, si el handshake probó que es él.
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

/// Vinculación sin pantalla: el token hace lo que allá hacen los seis dígitos.
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
        let _ = sess.send(&Msg::Reject {
            reason: "different key from the one already stored for this device".into(),
        });
        server
            .host
            .with_db(|conn| Ok(pair::log_key_mismatch(conn, uid, name)))?;
        return Err(anyhow!("different key for {uid}"));
    }

    // El token se compara entero (ver `pair::secret_eq`): con una comparación
    // que corta en la primera diferencia, el tiempo de respuesta filtra cuántos
    // caracteres del principio son correctos.
    let ok = token.map(|t| pair::secret_eq(t, &server.token)).unwrap_or(false);
    if !ok {
        let _ = sess.send(&Msg::PairResponse { accepted: false });
        log::warn!("[server] {name} ({uid}) tried to pair with a wrong token");
        return Err(anyhow!("invalid token"));
    }

    sess.send(&Msg::PairResponse { accepted: true })?;
    // El otro lado también tiene que aceptar: alcanza con que uno solo diga que
    // no para que no haya vínculo.
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

    // Presentación mutua, igual que entre dos dispositivos: los dos mandan
    // primero y después esperan, así que no se traban.
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

/// Un reloj corrido elige mal en el merge por LWW, y en un server que nadie
/// mira es exactamente el tipo de cosa que no se descubre hasta que ya pasó.
fn report_clock(name: &str, their_clock: i64) {
    let skew = their_clock - db::now_ms();
    if skew.abs() > 5 * 60 * 1000 {
        log::warn!("[server] clock of {name} is off by {skew} ms - last-write-wins may pick the wrong side");
    }
}
