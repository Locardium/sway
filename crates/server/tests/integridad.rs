//! Integridad del archivo: un server y dispositivos de verdad, sobre loopback.
//!
//! La Fase 5.8 dejó dos motores sincronizando en un mismo proceso para poder
//! probar lo que a mano no se prueba. Esto suma el tercer participante, que es
//! el que trae los casos nuevos: un dispositivo que perdió todo y lo recupera,
//! y —el que no puede fallar nunca— un dispositivo vacío que **no** convence al
//! archivo de que no había nada.
//!
//! Nada está simulado: el server es el mismo binario, con su socket, su base y
//! su hilo; los dispositivos hablan el protocolo real y mueven bytes reales.

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use sway_core::engine::{self, Host, Progress};
use sway_core::rusqlite::Connection;
use sway_core::wire::{Msg, Session};
use sway_core::{db, pairing};
use sway_server::host::ServerHost;
use sway_server::serve::{self, Server};

const TOKEN: &str = "token-de-la-suite";
/// Ninguna prueba mueve más de unos kilobytes: si algo se queda esperando es
/// un bug, y vale más un error que una suite colgada.
const IO_TIMEOUT: Duration = Duration::from_secs(20);

fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sway-int-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn mem_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    db::init_schema(&conn).unwrap();
    db::this_device_uid(&conn).unwrap();
    conn
}

// ---------------------------------------------------------------------------
// Un dispositivo
// ---------------------------------------------------------------------------

/// Lo mínimo que el motor necesita de un dispositivo real: una base y una
/// carpeta donde viven los archivos.
struct Device {
    dir: PathBuf,
    db: Mutex<Connection>,
}

impl Host for Device {
    fn with_db<T>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let conn = self.db.lock().map_err(|_| anyhow::anyhow!("db lock"))?;
        f(&conn)
    }
    fn music_dir(&self) -> PathBuf {
        self.dir.clone()
    }
    fn progress(&self, _p: &Progress) {}
}

impl Device {
    fn new(tag: &str) -> Self {
        Device {
            dir: tmpdir(tag),
            db: Mutex::new(mem_db()),
        }
    }

    fn uid(&self) -> String {
        self.with_db(|c| Ok(db::this_device_uid(c)?)).unwrap()
    }

    /// Un track con archivo real en la carpeta gestionada.
    fn add_track(&self, filename: &str, bytes: &[u8], title: &str) -> String {
        let path = self.dir.join(filename);
        std::fs::write(&path, bytes).unwrap();
        let hash = sway_core::hashing::hash_file(&path).unwrap();
        let uid = db::new_uid();
        let conn = self.db.lock().unwrap();
        sway_core::transfer::insert_received(
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
            db::now_ms(),
        )
        .unwrap();
        uid
    }

    fn delete_track(&self, uid: &str) {
        let conn = self.db.lock().unwrap();
        let path: String = conn
            .query_row("SELECT path FROM tracks WHERE uid = ?1", [uid], |r| r.get(0))
            .unwrap();
        conn.execute("DELETE FROM tracks WHERE uid = ?1", [uid]).unwrap();
        db::record_tombstone(&conn, "track", uid).unwrap();
        std::fs::remove_file(path).ok();
    }

    fn track_uids(&self) -> Vec<String> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn.prepare("SELECT uid FROM tracks ORDER BY uid").unwrap();
        let v: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        v
    }

    fn audio_files(&self) -> Vec<PathBuf> {
        audio_files_in(&self.dir)
    }

    /// Se vincula con el server y queda listo para sincronizar.
    fn pair_with(&self, addr: &str, server_uid: &str) {
        let mut sess = self.connect(addr);
        let (uid, name) = self.with_db(|c| Ok(pairing::me(c)?)).unwrap();
        sess.send(&Msg::PairRequest {
            uid,
            name,
            platform: "test".into(),
            token: Some(TOKEN.into()),
        })
        .unwrap();
        match sess.recv().unwrap() {
            Msg::PairResponse { accepted } => assert!(accepted, "el server no aceptó el token"),
            other => panic!("se esperaba PairResponse, llegó {other:?}"),
        }
        sess.send(&Msg::PairAck { accepted: true }).unwrap();
        let key = match sess.recv().unwrap() {
            Msg::Hello { .. } => sess.peer_pubkey.clone(),
            other => panic!("expected Hello, got {other:?}"),
        };
        sess.send(&self.hello()).unwrap();
        drop(sess);

        // Del lado del dispositivo el vínculo también se guarda: sin la fila,
        // sincronizar después sería hablar con un desconocido.
        self.with_db(|c| pairing::store_device(c, server_uid, "Server", "server", &key))
            .unwrap();
    }

    fn hello(&self) -> Msg {
        let (uid, name) = self.with_db(|c| Ok(pairing::me(c)?)).unwrap();
        let (tracks, playlists) = self.with_db(|c| Ok(pairing::library_counts(c))).unwrap();
        Msg::Hello {
            uid,
            name,
            platform: "test".into(),
            tracks,
            playlists,
            clock_ms: db::now_ms(),
        }
    }

    fn connect(&self, addr: &str) -> Session {
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
        stream.set_write_timeout(Some(IO_TIMEOUT)).unwrap();
        let private = self.with_db(|c| Ok(pairing::keypair(c)?.0)).unwrap();
        Session::connect(stream, &private).unwrap()
    }

    /// Una corrida completa contra el server, igual que la que dispara la app.
    fn sync_with(&self, addr: &str, server_uid: &str) -> engine::SyncResult {
        let mut sess = self.connect(addr);
        sess.send(&self.hello()).unwrap();
        match sess.recv().unwrap() {
            Msg::Hello { .. } => {}
            other => panic!("expected Hello, got {other:?}"),
        }
        let out = engine::sync(self, &mut sess, server_uid).unwrap();
        let _ = sess.send(&Msg::Bye);
        out
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

// ---------------------------------------------------------------------------
// El server
// ---------------------------------------------------------------------------

struct Archive {
    server: Arc<Server>,
    addr: String,
    dir: PathBuf,
}

impl Archive {
    fn start(tag: &str) -> Self {
        let dir = tmpdir(tag);
        let conn = mem_db();
        db::set_device_name(&conn, "Server de prueba").unwrap();
        let host = Arc::new(ServerHost::new(conn, dir.clone()));
        // Igual que el binario: el archivo se declara queriendo todo, en las
        // dos direcciones.
        host.with_db(|c| {
            let me = db::this_device_uid(c)?;
            sway_core::scope::set_mode(c, &me, sway_core::scope::Mode::All)?;
            sway_core::scope::set_direction(c, &me, "both")?;
            Ok(())
        })
        .unwrap();

        let server = Arc::new(Server {
            host,
            token: TOKEN.into(),
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let bg = Arc::clone(&server);
        std::thread::spawn(move || {
            let _ = serve::run(bg, listener);
        });
        Archive { server, addr, dir }
    }

    fn uid(&self) -> String {
        self.server
            .host
            .with_db(|c| Ok(db::this_device_uid(c)?))
            .unwrap()
    }

    fn track_titles(&self) -> Vec<String> {
        self.server
            .host
            .with_db(|c| {
                let mut stmt = c.prepare("SELECT title FROM tracks ORDER BY title")?;
                let v: Vec<String> = stmt
                    .query_map([], |r| r.get(0))?
                    .map(|r| r.unwrap())
                    .collect();
                Ok(v)
            })
            .unwrap()
    }

    fn audio_files(&self) -> Vec<PathBuf> {
        audio_files_in(&self.dir)
    }

    /// Lo que quedó en la papelera del server.
    fn trashed(&self) -> Vec<Vec<u8>> {
        std::fs::read_dir(sway_core::trash::trash_dir(&self.dir))
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().is_file())
                    .map(|e| std::fs::read(e.path()).unwrap())
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl Drop for Archive {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn audio_files_in(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().is_file())
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

fn audio(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| ((i + seed as usize) % 251) as u8).collect()
}

// ---------------------------------------------------------------------------

/// El caso base: lo que hay en un dispositivo termina en el archivo, con los
/// bytes, no sólo con la fila.
#[test]
fn lo_que_tiene_un_dispositivo_termina_en_el_archivo() {
    let srv = Archive::start("sube");
    let pc = Device::new("sube-pc");
    let srv_uid = srv.uid();
    pc.pair_with(&srv.addr, &srv_uid);

    pc.add_track("uno.flac", &audio(4096, 1), "Uno");
    pc.add_track("dos.flac", &audio(4096, 2), "Dos");
    let r = pc.sync_with(&srv.addr, &srv_uid);

    assert_eq!(r.sent, 2, "los dos archivos tenían que viajar");
    assert_eq!(srv.track_titles(), vec!["Dos", "Uno"]);
    assert_eq!(srv.audio_files().len(), 2, "y quedar en disco, no sólo en la base");
}

/// Sincronizar de nuevo sin haber tocado nada no mueve un byte. Converger una
/// vez es fácil; quedarse quieto después es lo que se rompe.
#[test]
fn la_segunda_corrida_no_mueve_nada() {
    let srv = Archive::start("quieto");
    let pc = Device::new("quieto-pc");
    let srv_uid = srv.uid();
    pc.pair_with(&srv.addr, &srv_uid);
    pc.add_track("uno.flac", &audio(2048, 3), "Uno");

    pc.sync_with(&srv.addr, &srv_uid);
    let segunda = pc.sync_with(&srv.addr, &srv_uid);

    assert_eq!((segunda.sent, segunda.received), (0, 0));
    assert_eq!(segunda.organized, 0, "tampoco organización");
}

/// Reinstalaste la app y no queda nada. Te vinculás de nuevo con el token y
/// sincronizás: vuelve todo, archivos incluidos.
#[test]
fn un_dispositivo_que_perdio_todo_lo_recupera_del_archivo() {
    let srv = Archive::start("restore");
    let srv_uid = srv.uid();

    let viejo = Device::new("restore-viejo");
    viejo.pair_with(&srv.addr, &srv_uid);
    viejo.add_track("uno.flac", &audio(4096, 11), "Uno");
    viejo.add_track("dos.flac", &audio(4096, 12), "Dos");
    viejo.sync_with(&srv.addr, &srv_uid);
    drop(viejo);

    // La reinstalación es un dispositivo nuevo: identidad nueva, base vacía,
    // carpeta vacía. Lo único que trae es el token.
    let nuevo = Device::new("restore-nuevo");
    nuevo.pair_with(&srv.addr, &srv_uid);
    assert!(nuevo.track_uids().is_empty());

    let r = nuevo.sync_with(&srv.addr, &srv_uid);

    assert_eq!(r.received, 2, "tenía que bajar los dos");
    assert_eq!(nuevo.track_uids().len(), 2);
    assert_eq!(nuevo.audio_files().len(), 2, "los archivos, no sólo las filas");
}

/// **El que no puede fallar nunca.**
///
/// Un dispositivo con la base vacía no tiene tombstones: no está diciendo que
/// algo se borró, está diciendo que no sabe nada. Si el archivo interpretara
/// ese silencio como un borrado, un teléfono reseteado se llevaría puesta la
/// única copia que quedaba de todo.
#[test]
fn un_dispositivo_vacio_no_borra_nada_del_archivo() {
    let srv = Archive::start("vacio");
    let srv_uid = srv.uid();

    let pc = Device::new("vacio-pc");
    pc.pair_with(&srv.addr, &srv_uid);
    pc.add_track("uno.flac", &audio(4096, 21), "Uno");
    pc.add_track("dos.flac", &audio(4096, 22), "Dos");
    pc.sync_with(&srv.addr, &srv_uid);
    assert_eq!(srv.track_titles().len(), 2);

    let reseteado = Device::new("vacio-reset");
    reseteado.pair_with(&srv.addr, &srv_uid);
    reseteado.sync_with(&srv.addr, &srv_uid);

    assert_eq!(srv.track_titles(), vec!["Dos", "Uno"], "el archivo se vació");
    assert_eq!(srv.audio_files().len(), 2, "y perdió los archivos");

    // Y el que quedó vivo tampoco pierde nada cuando vuelve a sincronizar.
    pc.sync_with(&srv.addr, &srv_uid);
    assert_eq!(pc.track_uids().len(), 2);
    assert_eq!(pc.audio_files().len(), 2);
}

/// Un borrado sí viaja, y en el server el archivo no se destruye: queda en su
/// papelera, que es lo único que hace rescatable un borrado por error.
#[test]
fn un_borrado_viaja_pero_el_archivo_queda_en_la_papelera_del_server() {
    let srv = Archive::start("borrado");
    let srv_uid = srv.uid();
    let pc = Device::new("borrado-pc");
    pc.pair_with(&srv.addr, &srv_uid);

    let bytes = audio(4096, 31);
    let uid = pc.add_track("uno.flac", &bytes, "Uno");
    pc.add_track("dos.flac", &audio(4096, 32), "Dos");
    pc.sync_with(&srv.addr, &srv_uid);
    assert_eq!(srv.track_titles().len(), 2);

    pc.delete_track(&uid);
    pc.sync_with(&srv.addr, &srv_uid);

    assert_eq!(srv.track_titles(), vec!["Dos"], "el borrado tenía que llegar");
    assert_eq!(srv.audio_files().len(), 1);
    assert!(
        srv.trashed().contains(&bytes),
        "pero los bytes siguen rescatables en la papelera del server"
    );

    // Y no resucita en la vuelta siguiente.
    let otra = pc.sync_with(&srv.addr, &srv_uid);
    assert_eq!((otra.sent, otra.received), (0, 0));
    assert_eq!(pc.track_uids().len(), 1);
}

/// Dos dispositivos que nunca se ven entre sí, cada uno en su red: el archivo
/// es lo que los conecta. Es el caso que motivó toda la fase.
#[test]
fn dos_dispositivos_que_no_se_ven_se_sincronizan_por_el_archivo() {
    let srv = Archive::start("puente");
    let srv_uid = srv.uid();

    let celu = Device::new("puente-celu");
    celu.pair_with(&srv.addr, &srv_uid);
    celu.add_track("de-afuera.flac", &audio(4096, 41), "De afuera");
    celu.sync_with(&srv.addr, &srv_uid);

    // El otro estaba apagado mientras todo esto pasaba.
    let pc = Device::new("puente-pc");
    pc.pair_with(&srv.addr, &srv_uid);
    let r = pc.sync_with(&srv.addr, &srv_uid);

    assert_eq!(r.received, 1);
    assert_eq!(pc.audio_files().len(), 1, "llegó sin que el celu estuviera");
}
