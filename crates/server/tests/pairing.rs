//! Pairing against the server, talking to it over the real protocol.
//!
//! The server is brought up for real —its socket, its database, its
//! thread— and on the other side there's a client doing exactly what the
//! app does: Noise handshake, `PairRequest`, introduction. No calling loose
//! functions and asserting on the result: what matters is that a device with
//! the token gets in and one without it doesn't.

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use sway_core::db;
use sway_core::engine::Host;
use sway_core::wire::{generate_keypair, Msg, Session};
use sway_server::host::ServerHost;
use sway_server::serve::{self, Server};

const TOKEN: &str = "a-test-token";
/// If something is left waiting it's a bug: an error is worth more than a
/// suite that hangs.
const IO_TIMEOUT: Duration = Duration::from_secs(20);

fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sway-srv-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Brings up a server with an in-memory database and returns where to connect to it.
fn start(tag: &str) -> (Arc<Server>, String) {
    let dir = tmpdir(tag);
    let conn = sway_core::rusqlite::Connection::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();
    db::this_device_uid(&conn).unwrap();
    db::set_device_name(&conn, "Test Server").unwrap();

    let server = Arc::new(Server {
        host: Arc::new(ServerHost::new(conn, dir)),
        token: TOKEN.into(),
    });
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let bg = Arc::clone(&server);
    std::thread::spawn(move || {
        let _ = serve::run(bg, listener);
    });
    (server, addr)
}

/// The app's side: opens the encrypted channel and is ready to talk.
fn connect(addr: &str) -> Session {
    let stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
    stream.set_write_timeout(Some(IO_TIMEOUT)).unwrap();
    let (private, _) = generate_keypair().unwrap();
    Session::connect(stream, &private).unwrap()
}

fn pair_request(token: Option<&str>) -> Msg {
    Msg::PairRequest {
        uid: "device-1".into(),
        name: "Phone".into(),
        platform: "android".into(),
        token: token.map(|t| t.to_string()),
    }
}

fn hello() -> Msg {
    Msg::Hello {
        uid: "device-1".into(),
        name: "Phone".into(),
        platform: "android".into(),
        tracks: 3,
        playlists: 1,
        clock_ms: db::now_ms(),
    }
}

/// How many devices the server has stored.
fn devices(server: &Server) -> i64 {
    server
        .host
        .with_db(|conn| {
            Ok(conn
                .query_row("SELECT COUNT(*) FROM devices", [], |r| r.get(0))
                .unwrap_or(-1))
        })
        .unwrap()
}

#[test]
fn with_the_right_token_the_device_gets_paired() {
    let (server, addr) = start("ok");
    let mut sess = connect(&addr);

    sess.send(&pair_request(Some(TOKEN))).unwrap();
    match sess.recv().unwrap() {
        Msg::PairResponse { accepted } => assert!(accepted, "the server rejected the correct token"),
        other => panic!("expected PairResponse, got {other:?}"),
    }
    sess.send(&Msg::PairAck { accepted: true }).unwrap();

    // Mutual introduction: the server sends its own and waits for ours.
    match sess.recv().unwrap() {
        Msg::Hello { platform, .. } => assert_eq!(platform, "server"),
        other => panic!("expected Hello, got {other:?}"),
    }
    sess.send(&hello()).unwrap();
    drop(sess);

    // The row is written on the server's thread, after sending the Hello.
    for _ in 0..100 {
        if devices(&server) == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(devices(&server), 1, "the device did not get stored");
}

#[test]
fn without_the_token_it_does_not_get_in() {
    let (server, addr) = start("mal");
    let mut sess = connect(&addr);

    sess.send(&pair_request(Some("wrong-token"))).unwrap();
    match sess.recv().unwrap() {
        Msg::PairResponse { accepted } => assert!(!accepted, "got in with the wrong token"),
        other => panic!("expected PairResponse, got {other:?}"),
    }
    drop(sess);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(devices(&server), 0, "stored a device that proved nothing");
}

#[test]
fn a_pair_request_without_a_token_does_not_get_in_either() {
    let (server, addr) = start("vacio");
    let mut sess = connect(&addr);

    // This is what the app sends when pairing with another device that has a
    // screen: there it's enough with the six-digit code, but here there's
    // nobody to look at it, so it has to be a no.
    sess.send(&pair_request(None)).unwrap();
    match sess.recv().unwrap() {
        Msg::PairResponse { accepted } => assert!(!accepted, "paired without any proof"),
        other => panic!("expected PairResponse, got {other:?}"),
    }
    drop(sess);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(devices(&server), 0);
}

#[test]
fn a_stranger_that_says_hello_finds_out_it_is_not_paired() {
    let (_server, addr) = start("hola");
    let mut sess = connect(&addr);

    sess.send(&hello()).unwrap();
    match sess.recv().unwrap() {
        Msg::NotPaired => {}
        other => panic!("expected NotPaired, got {other:?}"),
    }
}

#[test]
fn a_different_key_for_a_known_uid_gets_rejected() {
    let (server, addr) = start("clave");

    // Already paired, with a key different from the one the impostor is about to present.
    server
        .host
        .with_db(|conn| {
            sway_core::pairing::store_device(conn, "device-1", "Phone", "android", b"the-original")
        })
        .unwrap();

    let mut sess = connect(&addr);
    sess.send(&hello()).unwrap();
    match sess.recv().unwrap() {
        Msg::Reject { reason } => assert!(reason.contains("different key"), "reason: {reason}"),
        other => panic!("expected Reject, got {other:?}"),
    }

    // And it gets logged: a key that changes doesn't resolve itself.
    let logged: i64 = server
        .host
        .with_db(|conn| {
            Ok(conn
                .query_row(
                    "SELECT COUNT(*) FROM sync_log WHERE kind = 'key-mismatch'",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0))
        })
        .unwrap();
    assert_eq!(logged, 1);
}

#[test]
fn the_token_is_not_enough_if_the_key_does_not_match() {
    let (server, addr) = start("token-clave");
    server
        .host
        .with_db(|conn| {
            sway_core::pairing::store_device(conn, "device-1", "Phone", "android", b"the-original")
        })
        .unwrap();

    // It has the right token, but presents itself with the uid of an
    // already-paired device and a different key: this is exactly the shape
    // of the attack the token cannot solve, which is why the key is checked
    // first.
    let mut sess = connect(&addr);
    sess.send(&pair_request(Some(TOKEN))).unwrap();
    match sess.recv().unwrap() {
        Msg::Reject { reason } => assert!(reason.contains("different key"), "reason: {reason}"),
        other => panic!("expected Reject, got {other:?}"),
    }
}
