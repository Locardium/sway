//! Transferencia de archivos de audio (Fase 5.4).
//!
//! El requisito duro de toda la Fase 5 es que nunca se pierda música. Eso no
//! se consigue teniendo cuidado, se consigue con un camino que no tenga
//! forma de perder nada:
//!
//! ```text
//! 1. pedir el archivo desde el byte N (N = lo que ya haya en el .part)
//! 2. escribir en  <música>/.sway-incoming/<hash>.part
//! 3. corte de red / app matada -> el .part sobrevive, se reanuda desde ahí
//! 4. completo -> blake3 del .part ENTERO
//!      distinto  -> se borra el .part, la biblioteca nunca se tocó
//!      igual     -> rename() al destino final (atómico, mismo filesystem)
//! 5. recién ahí, la fila en la DB
//! ```
//!
//! Invariantes:
//! - El destino final **nunca** se escribe en vivo: todo pasa por el .part.
//! - Si el destino existe con otro contenido, se desambigua el nombre. Jamás
//!   se sobrescribe un archivo de audio existente.
//! - Se verifica el archivo completo, no sólo lo recién llegado: un .part con
//!   un prefijo corrupto de una corrida anterior también tiene que caer.

use crate::wire::{Msg, Session};
use anyhow::{anyhow, Result};
use rusqlite::OptionalExtension;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Tamaño de cada payload crudo. El canal lo trocea internamente en frames
/// de 64 KiB (tope de Noise); esto es sólo cuánto se lee del disco por vuelta.
const CHUNK: usize = 1024 * 1024;

/// Descargas a medio terminar. Está adentro de la carpeta gestionada a
/// propósito: `rename()` sólo es atómico dentro del mismo filesystem, y en
/// Android la carpeta de la app y la de música pueden estar en volúmenes
/// distintos.
pub fn incoming_dir(music_dir: &Path) -> PathBuf {
    music_dir.join(".sway-incoming")
}

fn part_path(music_dir: &Path, hash: &str) -> PathBuf {
    incoming_dir(music_dir).join(format!("{hash}.part"))
}

/// Cuánto hay ya descargado de este archivo.
fn resume_offset(music_dir: &Path, hash: &str) -> u64 {
    std::fs::metadata(part_path(music_dir, hash))
        .map(|m| m.len())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Lado que envía
// ---------------------------------------------------------------------------

/// Manda `path` desde `offset`. El receptor ya sabe el hash esperado.
pub fn send_file(sess: &mut Session, path: &Path, offset: u64, hash: &str) -> Result<()> {
    let mut f = std::fs::File::open(path)?;
    let total = f.metadata()?.len();
    if offset > total {
        return Err(anyhow!("offset {offset} más allá del archivo ({total})"));
    }
    f.seek(SeekFrom::Start(offset))?;
    sess.send(&Msg::BlobStart {
        size: total - offset,
    })?;

    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        sess.send_bytes(&buf[..n])?;
    }
    sess.send(&Msg::BlobEnd {
        hash: hash.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Lado que recibe
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Received {
    pub path: PathBuf,
    pub bytes: u64,
}

/// Recibe un archivo que ya fue pedido (o empujado) y lo deja en la carpeta
/// gestionada. Devuelve el path final.
///
/// `progress(recibidos, total)` se llama seguido: con archivos de 40 MB por
/// una red doméstica, sin esto la UI parece colgada.
/// `mark_expected` se llama con el destino **antes** del rename. El watcher de
/// la carpeta gestionada usa eso para no auto-importar lo que acaba de traer
/// el sync: si lo importara, crearía una fila con un uid nuevo y le pisaría
/// la metadata sincronizada con los tags del archivo.
///
/// `resuming` dice si lo que viene continúa un `.part` que ya estaba. **Sólo
/// es cierto cuando este lado pidió el archivo con un offset** (`pull_file`).
/// En un empuje el otro lado manda desde cero sin saber qué tenemos, así que
/// un `.part` viejo del mismo hash hay que tirarlo: apilarle el archivo entero
/// encima da un archivo más largo, hash distinto, y una transferencia entera a
/// la basura que se repite en cada sync.
pub fn receive_file(
    sess: &mut Session,
    music_dir: &Path,
    hash: &str,
    filename: &str,
    resuming: bool,
    progress: &mut dyn FnMut(u64, u64),
    mark_expected: &mut dyn FnMut(&Path),
) -> Result<Received> {
    let incoming = incoming_dir(music_dir);
    std::fs::create_dir_all(&incoming)?;
    let part = part_path(music_dir, hash);
    if !resuming && part.exists() {
        log::debug!("[sync] descarto un parcial viejo de {hash}");
        let _ = std::fs::remove_file(&part);
    }

    let expected = match sess.recv()? {
        Msg::BlobStart { size } => size,
        Msg::BlobError { reason } => return Err(anyhow!(reason)),
        other => return Err(anyhow!("se esperaba BlobStart, llegó {other:?}")),
    };

    // Append: lo que ya estaba se conserva; por eso el pedido llevaba offset.
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&part)?;
    let already = f.metadata()?.len();
    let total = already + expected;
    let mut got = already;
    progress(got, total);

    while got < total {
        let chunk = sess.recv_bytes()?;
        if chunk.is_empty() {
            return Err(anyhow!("el emisor cortó a los {got} de {total} bytes"));
        }
        f.write_all(&chunk)?;
        got += chunk.len() as u64;
        progress(got, total);
    }
    // Al disco antes de dar por buena la descarga: si se corta la luz acá, lo
    // que quede en el .part tiene que ser lo que realmente se escribió.
    f.flush()?;
    f.sync_all()?;
    drop(f);

    match sess.recv()? {
        Msg::BlobEnd { hash: sent } if sent == hash => {}
        Msg::BlobEnd { hash: sent } => {
            let _ = std::fs::remove_file(&part);
            return Err(anyhow!("el emisor dice haber mandado {sent}, se pedía {hash}"));
        }
        other => return Err(anyhow!("se esperaba BlobEnd, llegó {other:?}")),
    }

    // Se verifica el archivo ENTERO, no sólo lo que acaba de llegar: un .part
    // con un prefijo corrupto de una corrida anterior también tiene que caer.
    let actual = crate::hashing::hash_file(&part)?;
    if actual != hash {
        // La biblioteca nunca se tocó: lo único que se pierde es la descarga.
        let _ = std::fs::remove_file(&part);
        return Err(anyhow!("hash distinto (esperado {hash}, obtenido {actual})"));
    }

    let dest = crate::import::managed_dest_for(music_dir, filename, got);
    mark_expected(&dest);
    // `rename` dentro del mismo filesystem es atómico: el archivo aparece
    // entero o no aparece. Nunca queda uno a medias en la biblioteca.
    std::fs::rename(&part, &dest)?;
    Ok(Received {
        path: dest,
        bytes: got,
    })
}

/// Pide un archivo y lo recibe, reanudando si ya había algo bajado.
pub fn pull_file(
    sess: &mut Session,
    music_dir: &Path,
    hash: &str,
    filename: &str,
    progress: &mut dyn FnMut(u64, u64),
    mark_expected: &mut dyn FnMut(&Path),
) -> Result<Received> {
    let offset = resume_offset(music_dir, hash);
    if offset > 0 {
        log::info!("[sync] reanudando {filename} desde {offset} bytes");
    }
    sess.send(&Msg::BlobReq {
        hash: hash.to_string(),
        offset,
    })?;
    receive_file(sess, music_dir, hash, filename, offset > 0, progress, mark_expected)
}

// ---------------------------------------------------------------------------
// Alta en la biblioteca
// ---------------------------------------------------------------------------

/// Da de alta un archivo recibido conservando el `uid` del otro dispositivo.
///
/// El uid tiene que ser el mismo en los dos lados o el próximo sync no
/// reconocería que es la misma canción: las playlists y los tombstones lo
/// referencian. Por eso no se usa el camino de import normal, que genera uno
/// nuevo y relee los tags del archivo.
#[allow(clippy::too_many_arguments)]
pub fn insert_received(
    conn: &rusqlite::Connection,
    dest: &Path,
    uid: &str,
    hash: &str,
    size: u64,
    title: &str,
    artist: &str,
    album: &str,
    genre: &str,
    duration_ms: i64,
    bpm: Option<i64>,
    updated_at: i64,
) -> rusqlite::Result<i64> {
    let (_, mtime) = crate::hashing::file_stamp(dest).unwrap_or((0, 0));
    let rel = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Si ya hay una fila con este uid, NO se puede insertar otra: `uid` tiene
    // índice único y el INSERT revienta con "UNIQUE constraint failed:
    // tracks.uid". Y eso no es un detalle cosmético: el error corta la sesión
    // entera, así que el sync falla, se reintenta solo, y vuelve a fallar —
    // nada se copia nunca.
    //
    // Pasa más seguido de lo que parece: mientras el backfill de hashes no le
    // puso `content_hash` a una fila, el otro lado no la ve como presente y
    // manda el archivo que acá ya está.
    let existing: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, path FROM tracks WHERE uid = ?1",
            [uid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((id, old_path)) = existing {
        let old = Path::new(&old_path);
        if old != dest && old.exists() {
            // Ya había un archivo para este track. Hay que quedarse con uno
            // solo —el uid es único, la fila apunta a un path— y sobre todo
            // **dejar anotado un hash que corresponda al archivo que queda**.
            //
            // Antes esto descartaba la copia recibida sin tocar el hash. Si la
            // fila local no tenía hash (backfill pendiente), el otro lado
            // seguía viendo que nos "faltaba" y lo mandaba de nuevo: el mismo
            // tema viajando en cada sync, para siempre.
            let same = crate::hashing::hash_file(old).map(|h| h == hash).unwrap_or(false);
            if same {
                // Es el mismo contenido: la transferencia era redundante.
                let _ = std::fs::remove_file(dest);
                conn.execute(
                    "UPDATE tracks SET content_hash = ?1, size_bytes = ?2,
                            local_state = 'present' WHERE id = ?3",
                    rusqlite::params![hash, size as i64, id],
                )?;
                return Ok(id);
            }
            // Contenido distinto bajo el mismo track (otra codificación, otro
            // recorte). Gana el que llegó, y el que estaba **va a la papelera**
            // de la biblioteca, no al vacío: sigue siendo música del usuario y
            // se recupera por 30 días. Quedarse con el viejo no serviría —
            // el otro lado volvería a mandar el suyo en cada sync.
            // `dest` siempre cuelga de la carpeta gestionada (lo eligió
            // `managed_dest_for`), así que su padre ES la carpeta gestionada.
            if let Some(managed) = dest.parent() {
                match crate::trash::move_to_trash(managed, old) {
                    Ok(p) => {
                        log::info!("[sync] reemplazado, el anterior a la papelera: {}", p.display())
                    }
                    Err(e) => log::warn!("[sync] no se pudo archivar {}: {e}", old.display()),
                }
            }
        }
        // La fila apunta al archivo que acaba de llegar: o no tenía ninguno
        // (liberada por sync selectiva, borrado a mano) o el que tenía era otro
        // contenido y ya se archivó arriba.
        conn.execute(
            "UPDATE tracks SET path = ?1, rel_path = ?2, content_hash = ?3,
                    size_bytes = ?4, mtime_ms = ?5, local_state = 'present'
             WHERE id = ?6",
            rusqlite::params![
                dest.to_string_lossy(),
                rel,
                hash,
                size as i64,
                mtime,
                id
            ],
        )?;
        return Ok(id);
    }

    conn.execute(
        "INSERT INTO tracks (path, title, artist, album, genre, duration_ms, bpm,
                             uid, content_hash, rel_path, size_bytes, mtime_ms,
                             updated_at, local_state)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 'present')
         ON CONFLICT(path) DO UPDATE SET
            content_hash = excluded.content_hash,
            size_bytes   = excluded.size_bytes,
            mtime_ms     = excluded.mtime_ms,
            local_state  = 'present'",
        rusqlite::params![
            dest.to_string_lossy(),
            title,
            artist,
            album,
            genre,
            duration_ms,
            bpm,
            uid,
            hash,
            rel,
            size as i64,
            mtime,
            updated_at
        ],
    )?;
    conn.query_row(
        "SELECT id FROM tracks WHERE path = ?1",
        [dest.to_string_lossy()],
        |r| r.get(0),
    )
}

/// Resuelve el archivo local que corresponde a un hash, para poder servirlo.
pub fn path_for_hash(conn: &rusqlite::Connection, hash: &str) -> Option<PathBuf> {
    conn.query_row(
        "SELECT path FROM tracks WHERE content_hash = ?1 AND local_state = 'present' LIMIT 1",
        [hash],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .map(PathBuf::from)
    .filter(|p| p.exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::generate_keypair;
    use std::net::{TcpListener, TcpStream};

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sway-xfer-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Dos puntas de una sesión cifrada sobre loopback.
    fn pair() -> (Session, Session) {
        let (a, _) = generate_keypair().unwrap();
        let (b, _) = generate_keypair().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            Session::accept(s, &b).unwrap()
        });
        let client = Session::connect(TcpStream::connect(addr).unwrap(), &a).unwrap();
        (client, server.join().unwrap())
    }

    fn sample(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    /// Un empuje manda el archivo desde cero: el otro lado no sabe qué tenemos
    /// a medio bajar. Si el `.part` viejo se conservara, se le apilaría el
    /// archivo entero encima — más largo, otro hash, y una transferencia
    /// completa tirada que se repite en cada sync.
    #[test]
    fn a_stale_partial_does_not_poison_a_push() {
        let src_dir = tmpdir("push-src");
        let dst_dir = tmpdir("push-dst");
        let src = src_dir.join("track.flac");
        let data = sample(CHUNK + 500);
        std::fs::write(&src, &data).unwrap();
        let hash = crate::hashing::hash_file(&src).unwrap();

        // Sobra un parcial de una descarga anterior que se cortó.
        std::fs::create_dir_all(incoming_dir(&dst_dir)).unwrap();
        std::fs::write(part_path(&dst_dir, &hash), &data[..1000]).unwrap();

        let (mut client, mut server) = pair();
        let h = hash.clone();
        let src2 = src.clone();
        let sender = std::thread::spawn(move || {
            send_file(&mut server, &src2, 0, &h).unwrap();
        });
        let got = receive_file(
            &mut client,
            &dst_dir,
            &hash,
            "track.flac",
            false,
            &mut |_, _| {},
            &mut |_| {},
        )
        .expect("el parcial viejo no puede arruinar el empuje");
        sender.join().unwrap();

        assert_eq!(std::fs::read(&got.path).unwrap(), data);
        std::fs::remove_dir_all(&src_dir).ok();
        std::fs::remove_dir_all(&dst_dir).ok();
    }

    /// `uid` tiene índice único: recibir un track que acá ya existe no puede
    /// terminar en "UNIQUE constraint failed: tracks.uid". Ese error cortaba la
    /// sesión entera, así que el sync fallaba, se reintentaba solo y volvía a
    /// fallar — no se copiaba nada nunca. Pasa cada vez que el backfill de
    /// hashes todavía no le puso `content_hash` a la fila: el otro lado no la
    /// ve como presente y manda el archivo de nuevo.
    #[test]
    fn receiving_a_track_that_already_exists_here_does_not_break_the_session() {
        let dir = tmpdir("dup");
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::init_schema(&conn).unwrap();

        let mine = dir.join("ya-lo-tengo.flac");
        std::fs::write(&mine, b"audio").unwrap();
        conn.execute(
            "INSERT INTO tracks (path, uid, rel_path, local_state)
             VALUES (?1, 'uid-1', 'ya-lo-tengo.flac', 'present')",
            [mine.to_string_lossy()],
        )
        .unwrap();

        // Llega el mismo contenido con otro nombre de archivo.
        let hash = crate::hashing::hash_file(&mine).unwrap();
        let incoming = dir.join("ya-lo-tengo (2).flac");
        std::fs::write(&incoming, b"audio").unwrap();
        let id = insert_received(&conn, &incoming, "uid-1", &hash, 5, "T", "A", "", "", 0, None, 10)
            .expect("no puede fallar por el índice único");

        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "una sola fila para ese uid");
        let (path, stored): (String, Option<String>) = conn
            .query_row("SELECT path, content_hash FROM tracks WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(path, mine.to_string_lossy(), "se queda con el archivo que ya tenía");
        assert!(!incoming.exists(), "y la copia redundante no queda tirada");
        // Lo que cerraba el loop: sin hash anotado, el otro lado ve que nos
        // "falta" y lo vuelve a mandar en cada sync.
        assert_eq!(stored.as_deref(), Some(hash.as_str()));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Mismo track (mismo uid) con bytes distintos en cada lado. Hay que
    /// quedarse con uno solo o el sync no converge nunca — pero el que pierde
    /// **no se destruye**: va a la papelera de la biblioteca.
    #[test]
    fn a_different_encoding_of_the_same_track_does_not_loop_and_does_not_lose_the_old_file() {
        let dir = tmpdir("replace");
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::init_schema(&conn).unwrap();

        let mine = dir.join("tema.flac");
        std::fs::write(&mine, b"version vieja").unwrap();
        conn.execute(
            "INSERT INTO tracks (path, uid, rel_path, local_state)
             VALUES (?1, 'uid-1', 'tema.flac', 'present')",
            [mine.to_string_lossy()],
        )
        .unwrap();

        let incoming = dir.join("tema (2).flac");
        std::fs::write(&incoming, b"version nueva").unwrap();
        let hash = crate::hashing::hash_file(&incoming).unwrap();
        let id = insert_received(&conn, &incoming, "uid-1", &hash, 13, "T", "A", "", "", 0, None, 10)
            .unwrap();

        let (path, stored): (String, Option<String>) = conn
            .query_row("SELECT path, content_hash FROM tracks WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(path, incoming.to_string_lossy(), "gana el que llegó");
        assert_eq!(stored.as_deref(), Some(hash.as_str()), "y su hash queda anotado");
        assert!(!mine.exists(), "el viejo sale de la biblioteca");
        let rescatable = std::fs::read_dir(crate::trash::trash_dir(&dir))
            .unwrap()
            .flatten()
            .any(|e| std::fs::read(e.path()).unwrap() == b"version vieja");
        assert!(rescatable, "pero sigue existiendo en la papelera");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Ciclo completo de la sync selectiva: se libera el espacio (la fila
    /// queda `absent`) y al re-marcar la playlist el archivo vuelve **a la
    /// misma fila**. Reinsertar con el mismo uid y otro path chocaba contra el
    /// índice único de `uid`, y el track volvía duplicado o no volvía.
    #[test]
    fn a_freed_track_comes_back_to_the_same_row() {
        let dir = tmpdir("recover");
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::init_schema(&conn).unwrap();

        let first = dir.join("tema.flac");
        std::fs::write(&first, b"audio").unwrap();
        insert_received(&conn, &first, "uid-1", "h1", 5, "T", "A", "", "", 0, None, 10).unwrap();

        // Liberado: la fila se queda, el archivo no.
        conn.execute(
            "UPDATE tracks SET local_state = 'absent' WHERE uid = 'uid-1'",
            [],
        )
        .unwrap();
        std::fs::remove_file(&first).unwrap();

        // Vuelve, y con otro nombre (el destino se desambigua si hace falta).
        let again = dir.join("tema (2).flac");
        std::fs::write(&again, b"audio").unwrap();
        insert_received(&conn, &again, "uid-1", "h1", 5, "T", "A", "", "", 0, None, 10).unwrap();

        let (n, state, path): (i64, String, String) = conn
            .query_row(
                "SELECT COUNT(*), MAX(local_state), MAX(path) FROM tracks WHERE uid = 'uid-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(n, 1, "una sola fila, no un duplicado");
        assert_eq!(state, "present");
        assert_eq!(path, again.to_string_lossy());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn transfers_and_verifies_a_file() {
        let src_dir = tmpdir("src");
        let dst_dir = tmpdir("dst");
        let src = src_dir.join("track.flac");
        // Más de un chunk, para ejercitar el troceo.
        let data = sample(CHUNK * 2 + 1234);
        std::fs::write(&src, &data).unwrap();
        let hash = crate::hashing::hash_file(&src).unwrap();

        let (mut client, mut server) = pair();
        let h = hash.clone();
        let sender = std::thread::spawn(move || {
            match server.recv().unwrap() {
                Msg::BlobReq { offset, .. } => {
                    send_file(&mut server, &src, offset, &h).unwrap();
                }
                other => panic!("inesperado: {other:?}"),
            }
        });

        let got = pull_file(&mut client, &dst_dir, &hash, "track.flac", &mut |_, _| {}, &mut |_| {}).unwrap();
        sender.join().unwrap();

        assert_eq!(std::fs::read(&got.path).unwrap(), data);
        assert_eq!(got.bytes as usize, data.len());
        // El .part se consumió: no queda basura.
        assert!(!part_path(&dst_dir, &hash).exists());
        std::fs::remove_dir_all(&src_dir).ok();
        std::fs::remove_dir_all(&dst_dir).ok();
    }

    /// Un emisor que manda bytes que no son los del hash pedido —bug,
    /// corrupción en tránsito o mala fe— no puede meter nada en la
    /// biblioteca. Es el único chequeo que importa: el hash declarado en el
    /// BlobEnd no prueba nada, lo que se verifica son los bytes recibidos.
    #[test]
    fn content_that_does_not_match_the_requested_hash_is_rejected() {
        let src_dir = tmpdir("bad-src");
        let dst_dir = tmpdir("bad-dst");
        // Lo que el receptor quiere...
        let wanted = src_dir.join("bueno.flac");
        std::fs::write(&wanted, sample(5000)).unwrap();
        let wanted_hash = crate::hashing::hash_file(&wanted).unwrap();
        // ...y lo que el emisor manda en su lugar, diciendo que es lo mismo.
        let other = src_dir.join("otro.flac");
        std::fs::write(&other, sample(4096)).unwrap();

        let (mut client, mut server) = pair();
        let claimed = wanted_hash.clone();
        let sender = std::thread::spawn(move || {
            let offset = match server.recv().unwrap() {
                Msg::BlobReq { offset, .. } => offset,
                other => panic!("inesperado: {other:?}"),
            };
            send_file(&mut server, &other, offset, &claimed).unwrap();
        });

        let err = pull_file(&mut client, &dst_dir, &wanted_hash, "bueno.flac", &mut |_, _| {}, &mut |_| {})
            .unwrap_err();
        sender.join().unwrap();

        assert!(err.to_string().contains("hash distinto"), "error inesperado: {err}");
        // Ni archivo final ni .part: la biblioteca quedó intacta.
        assert!(!dst_dir.join("bueno.flac").exists());
        assert!(!part_path(&dst_dir, &wanted_hash).exists());
        std::fs::remove_dir_all(&src_dir).ok();
        std::fs::remove_dir_all(&dst_dir).ok();
    }

    /// Y si además miente en el BlobEnd, cae antes todavía.
    #[test]
    fn a_wrong_hash_in_blob_end_is_rejected() {
        let src_dir = tmpdir("end-src");
        let dst_dir = tmpdir("end-dst");
        let src = src_dir.join("x.flac");
        std::fs::write(&src, sample(2000)).unwrap();
        let hash = crate::hashing::hash_file(&src).unwrap();

        let (mut client, mut server) = pair();
        let sender = std::thread::spawn(move || {
            let offset = match server.recv().unwrap() {
                Msg::BlobReq { offset, .. } => offset,
                other => panic!("inesperado: {other:?}"),
            };
            send_file(&mut server, &src, offset, &"0".repeat(64)).unwrap();
        });
        let err = pull_file(&mut client, &dst_dir, &hash, "x.flac", &mut |_, _| {}, &mut |_| {}).unwrap_err();
        sender.join().unwrap();

        assert!(err.to_string().contains("dice haber mandado"), "error: {err}");
        assert!(!part_path(&dst_dir, &hash).exists());
        std::fs::remove_dir_all(&src_dir).ok();
        std::fs::remove_dir_all(&dst_dir).ok();
    }

    /// Una transferencia cortada por la mitad deja un .part, y la siguiente
    /// arranca desde ahí en vez de bajar todo de nuevo.
    #[test]
    fn an_interrupted_transfer_resumes_from_where_it_stopped() {
        let src_dir = tmpdir("res-src");
        let dst_dir = tmpdir("res-dst");
        let src = src_dir.join("largo.flac");
        let data = sample(CHUNK * 3);
        std::fs::write(&src, &data).unwrap();
        let hash = crate::hashing::hash_file(&src).unwrap();

        // Simula el corte: ya hay medio archivo bajado de una corrida previa.
        std::fs::create_dir_all(incoming_dir(&dst_dir)).unwrap();
        let half = data.len() / 2;
        std::fs::write(part_path(&dst_dir, &hash), &data[..half]).unwrap();

        let (mut client, mut server) = pair();
        let h = hash.clone();
        let asked = std::thread::spawn(move || {
            let offset = match server.recv().unwrap() {
                Msg::BlobReq { offset, .. } => offset,
                other => panic!("inesperado: {other:?}"),
            };
            send_file(&mut server, &src, offset, &h).unwrap();
            offset
        });

        let got = pull_file(&mut client, &dst_dir, &hash, "largo.flac", &mut |_, _| {}, &mut |_| {}).unwrap();
        let offset = asked.join().unwrap();

        assert_eq!(offset, half as u64, "tenía que pedir sólo lo que faltaba");
        // Y el archivo reensamblado es idéntico al original.
        assert_eq!(std::fs::read(&got.path).unwrap(), data);
        std::fs::remove_dir_all(&src_dir).ok();
        std::fs::remove_dir_all(&dst_dir).ok();
    }

    /// Un .part con el prefijo corrupto (disco que falló, corte sucio) no se
    /// puede detectar por tamaño: sólo cae al verificar el archivo completo.
    #[test]
    fn a_corrupted_resume_prefix_is_caught_by_the_full_hash() {
        let src_dir = tmpdir("pre-src");
        let dst_dir = tmpdir("pre-dst");
        let src = src_dir.join("x.flac");
        let data = sample(CHUNK + 500);
        std::fs::write(&src, &data).unwrap();
        let hash = crate::hashing::hash_file(&src).unwrap();

        let half = data.len() / 2;
        let mut bad = data[..half].to_vec();
        bad[10] ^= 0xFF; // un byte cambiado, mismo tamaño
        std::fs::create_dir_all(incoming_dir(&dst_dir)).unwrap();
        std::fs::write(part_path(&dst_dir, &hash), &bad).unwrap();

        let (mut client, mut server) = pair();
        let h = hash.clone();
        let sender = std::thread::spawn(move || {
            let offset = match server.recv().unwrap() {
                Msg::BlobReq { offset, .. } => offset,
                other => panic!("inesperado: {other:?}"),
            };
            send_file(&mut server, &src, offset, &h).unwrap();
        });

        let err =
            pull_file(&mut client, &dst_dir, &hash, "x.flac", &mut |_, _| {}, &mut |_| {}).unwrap_err();
        sender.join().unwrap();
        assert!(err.to_string().contains("hash"), "error inesperado: {err}");
        assert!(!part_path(&dst_dir, &hash).exists(), "el .part malo se descarta");
        std::fs::remove_dir_all(&src_dir).ok();
        std::fs::remove_dir_all(&dst_dir).ok();
    }

    /// Nunca se pisa un archivo existente: si el nombre está ocupado por otro
    /// contenido, el que llega se desambigua.
    #[test]
    fn an_existing_file_with_the_same_name_is_never_overwritten() {
        let src_dir = tmpdir("dup-src");
        let dst_dir = tmpdir("dup-dst");
        let existing = dst_dir.join("mismo.flac");
        std::fs::write(&existing, b"lo que ya estaba").unwrap();

        let src = src_dir.join("mismo.flac");
        let data = sample(3000);
        std::fs::write(&src, &data).unwrap();
        let hash = crate::hashing::hash_file(&src).unwrap();

        let (mut client, mut server) = pair();
        let h = hash.clone();
        let sender = std::thread::spawn(move || {
            let offset = match server.recv().unwrap() {
                Msg::BlobReq { offset, .. } => offset,
                other => panic!("inesperado: {other:?}"),
            };
            send_file(&mut server, &src, offset, &h).unwrap();
        });
        let got = pull_file(&mut client, &dst_dir, &hash, "mismo.flac", &mut |_, _| {}, &mut |_| {}).unwrap();
        sender.join().unwrap();

        assert_ne!(got.path, existing);
        assert_eq!(std::fs::read(&existing).unwrap(), b"lo que ya estaba");
        assert_eq!(std::fs::read(&got.path).unwrap(), data);
        std::fs::remove_dir_all(&src_dir).ok();
        std::fs::remove_dir_all(&dst_dir).ok();
    }
}
