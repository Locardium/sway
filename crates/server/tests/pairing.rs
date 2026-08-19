//! Vinculación contra el server, hablándole por el protocolo real.
//!
//! El server se levanta de verdad —su socket, su base, su hilo— y del otro
//! lado hay un cliente que hace exactamente lo que hace la app: handshake
//! Noise, `PairRequest`, presentación. Nada de llamar funciones sueltas y
//! afirmar sobre el resultado: lo que importa es que un dispositivo con el
//! token entre y uno sin el token no.

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use sway_core::db;
use sway_core::engine::Host;
use sway_core::wire::{generate_keypair, Msg, Session};
use sway_server::host::ServerHost;
use sway_server::serve::{self, Server};

const TOKEN: &str = "un-token-de-prueba";
/// Si algo se queda esperando es un bug: vale más un error que una suite
/// colgada.
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

/// Levanta un server con base en memoria y devuelve a dónde conectarse.
fn start(tag: &str) -> (Arc<Server>, String) {
    let dir = tmpdir(tag);
    let conn = sway_core::rusqlite::Connection::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();
    db::this_device_uid(&conn).unwrap();
    db::set_device_name(&conn, "Server de prueba").unwrap();

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

/// El lado de la app: abre el canal cifrado y queda listo para hablar.
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
        name: "Celu".into(),
        platform: "android".into(),
        token: token.map(|t| t.to_string()),
    }
}

fn hello() -> Msg {
    Msg::Hello {
        uid: "device-1".into(),
        name: "Celu".into(),
        platform: "android".into(),
        tracks: 3,
        playlists: 1,
        clock_ms: db::now_ms(),
    }
}

/// Cuántos dispositivos tiene guardados el server.
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
fn con_el_token_correcto_el_dispositivo_queda_vinculado() {
    let (server, addr) = start("ok");
    let mut sess = connect(&addr);

    sess.send(&pair_request(Some(TOKEN))).unwrap();
    match sess.recv().unwrap() {
        Msg::PairResponse { accepted } => assert!(accepted, "el server rechazó el token bueno"),
        other => panic!("se esperaba PairResponse, llegó {other:?}"),
    }
    sess.send(&Msg::PairAck { accepted: true }).unwrap();

    // Presentación mutua: el server manda el suyo y espera el nuestro.
    match sess.recv().unwrap() {
        Msg::Hello { platform, .. } => assert_eq!(platform, "server"),
        other => panic!("expected Hello, got {other:?}"),
    }
    sess.send(&hello()).unwrap();
    drop(sess);

    // La fila se escribe en el hilo del server, después de mandar el Hello.
    for _ in 0..100 {
        if devices(&server) == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(devices(&server), 1, "el dispositivo no quedó guardado");
}

#[test]
fn sin_el_token_no_entra() {
    let (server, addr) = start("mal");
    let mut sess = connect(&addr);

    sess.send(&pair_request(Some("token-equivocado"))).unwrap();
    match sess.recv().unwrap() {
        Msg::PairResponse { accepted } => assert!(!accepted, "entró con un token que no es"),
        other => panic!("se esperaba PairResponse, llegó {other:?}"),
    }
    drop(sess);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(devices(&server), 0, "guardó un dispositivo que no probó nada");
}

#[test]
fn un_pair_request_sin_token_tampoco_entra() {
    let (server, addr) = start("vacio");
    let mut sess = connect(&addr);

    // Es lo que manda la app cuando se vincula con otro dispositivo con
    // pantalla: allá alcanza el código de seis dígitos, acá no hay quién lo
    // mire, así que tiene que ser un no.
    sess.send(&pair_request(None)).unwrap();
    match sess.recv().unwrap() {
        Msg::PairResponse { accepted } => assert!(!accepted, "vinculó sin ninguna prueba"),
        other => panic!("se esperaba PairResponse, llegó {other:?}"),
    }
    drop(sess);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(devices(&server), 0);
}

#[test]
fn un_desconocido_que_saluda_se_entera_de_que_no_esta_vinculado() {
    let (_server, addr) = start("hola");
    let mut sess = connect(&addr);

    sess.send(&hello()).unwrap();
    match sess.recv().unwrap() {
        Msg::NotPaired => {}
        other => panic!("se esperaba NotPaired, llegó {other:?}"),
    }
}

#[test]
fn una_clave_distinta_para_un_uid_conocido_se_rechaza() {
    let (server, addr) = start("clave");

    // Ya vinculado, con una clave que no es la que va a presentar el impostor.
    server
        .host
        .with_db(|conn| {
            sway_core::pairing::store_device(conn, "device-1", "Celu", "android", b"la-original")
        })
        .unwrap();

    let mut sess = connect(&addr);
    sess.send(&hello()).unwrap();
    match sess.recv().unwrap() {
        Msg::Reject { reason } => assert!(reason.contains("different key"), "motivo: {reason}"),
        other => panic!("se esperaba Reject, llegó {other:?}"),
    }

    // Y queda anotado: una clave que cambia no se resuelve sola.
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
fn el_token_no_alcanza_si_la_clave_no_coincide() {
    let (server, addr) = start("token-clave");
    server
        .host
        .with_db(|conn| {
            sway_core::pairing::store_device(conn, "device-1", "Celu", "android", b"la-original")
        })
        .unwrap();

    // Tiene el token bueno, pero se presenta con el uid de un dispositivo ya
    // vinculado y otra clave: es exactamente la forma del ataque que el token
    // no puede resolver, y por eso la clave se mira primero.
    let mut sess = connect(&addr);
    sess.send(&pair_request(Some(TOKEN))).unwrap();
    match sess.recv().unwrap() {
        Msg::Reject { reason } => assert!(reason.contains("different key"), "motivo: {reason}"),
        other => panic!("se esperaba Reject, llegó {other:?}"),
    }
}
