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
pub fn receive_file(
    sess: &mut Session,
    music_dir: &Path,
    hash: &str,
    filename: &str,
    progress: &mut dyn FnMut(u64, u64),
    mark_expected: &mut dyn FnMut(&Path),
) -> Result<Received> {
    let incoming = incoming_dir(music_dir);
    std::fs::create_dir_all(&incoming)?;
    let part = part_path(music_dir, hash);

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
    receive_file(sess, music_dir, hash, filename, progress, mark_expected)
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
