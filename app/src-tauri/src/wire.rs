//! Canal cifrado entre dos dispositivos Sway (Fase 5.2).
//!
//! Handshake **Noise XX** (`Noise_XX_25519_ChaChaPoly_BLAKE2s`) sobre TCP.
//! XX intercambia las claves estaticas de los dos lados, asi que despues del
//! handshake cada uno sabe con que clave publica esta hablando — pero todavia
//! no si esa clave es de quien dice ser. Eso lo resuelve el codigo de
//! verificacion (ver `sas_code`), no la criptografia.
//!
//! Se eligio Noise sobre TLS porque no hay que montar PKI: cada dispositivo
//! tiene un par de claves y nada mas. Ni certificados que generar, ni cadenas
//! que validar, ni un almacen de confianza que mantener en Android.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use snow::{Builder, HandshakeState, TransportState};
use std::io::{Read, Write};
use std::net::TcpStream;

const PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";

/// Tope de Noise para un mensaje (64 KiB), menos los 16 bytes del tag.
const MAX_NOISE_PAYLOAD: usize = 65535 - 16;
/// Cota de cordura sobre un mensaje logico entrante: evita que un peer
/// hostil (o un bug) haga reservar memoria sin limite.
const MAX_MESSAGE: usize = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Mensajes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Msg {
    /// Primer mensaje cuando el que llama todavia no esta pareado.
    PairRequest {
        uid: String,
        name: String,
        platform: String,
    },
    /// Decision del lado que recibe.
    PairResponse { accepted: bool },
    /// Decision del lado que llama. El pairing se concreta solo si los DOS
    /// aceptaron: alcanza con que uno vea un codigo distinto para cortar.
    PairAck { accepted: bool },
    /// Presentacion entre dispositivos ya pareados.
    Hello {
        uid: String,
        name: String,
        platform: String,
        tracks: i64,
        playlists: i64,
        clock_ms: i64,
    },
    /// Corte ordenado con motivo, para poder mostrarlo en la UI del otro lado.
    Reject { reason: String },
    /// "Te saqué de mis dispositivos". El pairing se guarda de los dos lados,
    /// asi que desvincular tiene que avisar o el otro sigue creyendo que
    /// estan vinculados para siempre.
    Unpair { uid: String },
    /// "No te tengo en mi lista." Lo manda quien recibe un `Hello` de un peer
    /// que no conoce; el que llama lo usa para darse cuenta de que lo
    /// desvincularon del otro lado.
    NotPaired,
    /// Pedido y respuesta del inventario de la biblioteca (Fase 5.3).
    ManifestReq,
    ManifestData {
        manifest: Box<crate::manifest::Manifest>,
    },
    // --- Transferencia de archivos (Fase 5.4) -----------------------------
    //
    // Los bytes NO viajan adentro de estos mensajes: van como payloads crudos
    // por el mismo canal (`Session::send_bytes`). Meterlos en el JSON
    // costaria 1.33x en base64 o 3-4x como array de numeros, sobre gigabytes.
    // El protocolo es: BlobStart -> N payloads crudos -> BlobEnd.
    /// Pedido de un archivo por su hash. `offset` != 0 es una reanudacion.
    BlobReq { hash: String, offset: u64 },
    /// Empieza el envio: `size` es lo que queda por mandar desde el offset.
    BlobStart { size: u64 },
    /// Fin del envio; `hash` es el del archivo COMPLETO, para verificar.
    BlobEnd { hash: String },
    /// Empuje en la otra direccion: "tomá este archivo". Le siguen los
    /// payloads crudos y un BlobEnd, igual que arriba.
    BlobPush {
        track_uid: String,
        hash: String,
        filename: String,
        size: u64,
        title: String,
        artist: String,
        album: String,
        genre: String,
        duration_ms: i64,
        bpm: Option<i64>,
        updated_at: i64,
    },
    /// El otro lado no puede servir lo pedido (no lo tiene, no lo puede leer).
    BlobError { reason: String },
    /// Metadata, playlists, carpetas y membresias que le faltan al otro lado
    /// (Fase 5.5). Va despues de los archivos: una membresia de un track que
    /// todavia no llego se ignora, asi que primero conviene que exista.
    MetaPush {
        changes: Box<crate::merge::Changes>,
    },
    /// Cuantos registros se aplicaron de verdad del otro lado.
    MetaAck {
        applied: crate::merge::Applied,
    },
    /// Cierre ordenado de la sesion.
    Bye,
}

// ---------------------------------------------------------------------------
// Codigo de verificacion
// ---------------------------------------------------------------------------

/// Seis digitos derivados del hash del handshake.
///
/// Ese hash depende de las claves efimeras y estaticas de los dos lados, asi
/// que un intermediario que se meta en el medio termina con DOS sesiones
/// distintas y dos hashes distintos: no puede hacer que los dos codigos
/// coincidan. Que el usuario compare los numeros en las dos pantallas es lo
/// que convierte "hay un canal cifrado con alguien" en "hay un canal cifrado
/// con este dispositivo".
pub fn sas_code(handshake_hash: &[u8]) -> String {
    let mut n: u32 = 0;
    for b in handshake_hash.iter().take(4) {
        n = (n << 8) | *b as u32;
    }
    format!("{:06}", n % 1_000_000)
}

// ---------------------------------------------------------------------------
// Claves estaticas
// ---------------------------------------------------------------------------

/// Genera un par de claves nuevo (una sola vez por dispositivo).
pub fn generate_keypair() -> Result<(Vec<u8>, Vec<u8>)> {
    let kp = Builder::new(PARAMS.parse()?).generate_keypair()?;
    Ok((kp.private, kp.public))
}

// ---------------------------------------------------------------------------
// Sesion
// ---------------------------------------------------------------------------

pub struct Session {
    stream: TcpStream,
    noise: TransportState,
    /// Clave publica estatica del otro lado, aprendida en el handshake.
    pub peer_pubkey: Vec<u8>,
    /// Codigo a mostrar en pantalla. Los dos lados calculan el mismo.
    pub code: String,
}

impl Session {
    /// Lado que llama.
    pub fn connect(stream: TcpStream, private_key: &[u8]) -> Result<Self> {
        let mut hs = Builder::new(PARAMS.parse()?)
            .local_private_key(private_key)?
            .build_initiator()?;
        let mut buf = vec![0u8; 65535];

        // XX es de tres mensajes: -> e | <- e ee s es | -> s se
        let n = hs.write_message(&[], &mut buf)?;
        write_frame(&stream, &buf[..n])?;
        let msg = read_frame(&stream)?;
        hs.read_message(&msg, &mut buf)?;
        let n = hs.write_message(&[], &mut buf)?;
        write_frame(&stream, &buf[..n])?;

        Self::finish(stream, hs)
    }

    /// Lado que acepta.
    pub fn accept(stream: TcpStream, private_key: &[u8]) -> Result<Self> {
        let mut hs = Builder::new(PARAMS.parse()?)
            .local_private_key(private_key)?
            .build_responder()?;
        let mut buf = vec![0u8; 65535];

        let msg = read_frame(&stream)?;
        hs.read_message(&msg, &mut buf)?;
        let n = hs.write_message(&[], &mut buf)?;
        write_frame(&stream, &buf[..n])?;
        let msg = read_frame(&stream)?;
        hs.read_message(&msg, &mut buf)?;

        Self::finish(stream, hs)
    }

    fn finish(stream: TcpStream, hs: HandshakeState) -> Result<Self> {
        // El hash del handshake hay que leerlo ANTES de pasar a transporte:
        // `into_transport_mode` consume el estado.
        let code = sas_code(hs.get_handshake_hash());
        let peer_pubkey = hs
            .get_remote_static()
            .ok_or_else(|| anyhow!("el peer no mando su clave estatica"))?
            .to_vec();
        Ok(Self {
            stream,
            noise: hs.into_transport_mode()?,
            peer_pubkey,
            code,
        })
    }

    pub fn send(&mut self, msg: &Msg) -> Result<()> {
        self.send_bytes(&serde_json::to_vec(msg)?)
    }

    pub fn recv(&mut self) -> Result<Msg> {
        let bytes = self.recv_bytes()?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Un mensaje logico va como un frame de cabecera con el largo total, y
    /// despues los frames de datos. Noise no puede cifrar mas de 64 KiB de
    /// una, y los manifests (5.3) y los bloques de audio (5.4) pasan ese
    /// tamaño de sobra, asi que el troceo se resuelve aca abajo una vez.
    pub fn send_bytes(&mut self, data: &[u8]) -> Result<()> {
        let mut buf = vec![0u8; 65535];
        let header = (data.len() as u32).to_be_bytes();
        let n = self.noise.write_message(&header, &mut buf)?;
        write_frame(&self.stream, &buf[..n])?;
        for chunk in data.chunks(MAX_NOISE_PAYLOAD) {
            let n = self.noise.write_message(chunk, &mut buf)?;
            write_frame(&self.stream, &buf[..n])?;
        }
        self.stream.flush()?;
        Ok(())
    }

    pub fn recv_bytes(&mut self) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; 65535];
        let frame = read_frame(&self.stream)?;
        let n = self.noise.read_message(&frame, &mut buf)?;
        if n != 4 {
            return Err(anyhow!("cabecera invalida ({n} bytes)"));
        }
        let total = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if total > MAX_MESSAGE {
            return Err(anyhow!("mensaje demasiado grande ({total} bytes)"));
        }
        let mut out = Vec::with_capacity(total);
        while out.len() < total {
            let frame = read_frame(&self.stream)?;
            let n = self.noise.read_message(&frame, &mut buf)?;
            out.extend_from_slice(&buf[..n]);
            if n == 0 {
                return Err(anyhow!("el peer corto el mensaje por la mitad"));
            }
        }
        out.truncate(total);
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Framing en el socket: [u32 largo][bytes]
// ---------------------------------------------------------------------------

fn write_frame(mut stream: &TcpStream, data: &[u8]) -> Result<()> {
    stream.write_all(&(data.len() as u32).to_be_bytes())?;
    stream.write_all(data)?;
    Ok(())
}

fn read_frame(mut stream: &TcpStream) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > 65535 {
        return Err(anyhow!("frame invalido ({len} bytes)"));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    /// Levanta las dos puntas de una sesion sobre loopback.
    fn pair() -> (Session, Session) {
        let (a_priv, _) = generate_keypair().unwrap();
        let (b_priv, _) = generate_keypair().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            Session::accept(stream, &b_priv).unwrap()
        });
        let client = Session::connect(TcpStream::connect(addr).unwrap(), &a_priv).unwrap();
        (client, server.join().unwrap())
    }

    #[test]
    fn both_sides_derive_the_same_code_and_see_each_others_keys() {
        let (a, b) = pair();
        assert_eq!(a.code, b.code);
        assert_eq!(a.code.len(), 6);
        assert!(a.code.chars().all(|c| c.is_ascii_digit()));
        // Cada lado aprendio la clave estatica del otro, que es lo que despues
        // se fija en `devices`.
        assert_ne!(a.peer_pubkey, b.peer_pubkey);
        assert_eq!(a.peer_pubkey.len(), 32);
    }

    /// Dos sesiones distintas tienen que dar codigos distintos: si el codigo
    /// no dependiera de las claves efimeras, un intermediario podria replicar
    /// el mismo numero en las dos pantallas.
    #[test]
    fn code_differs_between_sessions() {
        let (a1, _) = pair();
        let (a2, _) = pair();
        assert_ne!(a1.code, a2.code);
    }

    #[test]
    fn messages_round_trip() {
        let (mut a, mut b) = pair();
        a.send(&Msg::PairRequest {
            uid: "u-1".into(),
            name: "PC".into(),
            platform: "windows".into(),
        })
        .unwrap();
        match b.recv().unwrap() {
            Msg::PairRequest { uid, name, .. } => {
                assert_eq!(uid, "u-1");
                assert_eq!(name, "PC");
            }
            other => panic!("mensaje inesperado: {other:?}"),
        }
        b.send(&Msg::PairResponse { accepted: true }).unwrap();
        match a.recv().unwrap() {
            Msg::PairResponse { accepted } => assert!(accepted),
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }

    /// Un manifest (5.3) o un bloque de audio (5.4) pasan los 64 KiB que
    /// Noise cifra de una: el troceo tiene que ser transparente.
    #[test]
    fn payloads_larger_than_one_noise_message_survive() {
        let (mut a, mut b) = pair();
        let big: Vec<u8> = (0..500_000).map(|i| (i % 251) as u8).collect();
        let sender = std::thread::spawn(move || {
            a.send_bytes(&big).unwrap();
            big
        });
        let got = b.recv_bytes().unwrap();
        assert_eq!(got, sender.join().unwrap());
    }
}
