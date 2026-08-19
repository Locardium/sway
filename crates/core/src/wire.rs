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

/// Hasta donde sabe un dispositivo de la biblioteca del otro.
///
/// `epoch` identifica la corrida del que atiende, y no es decoracion: la
/// cuenta de revisiones vive en memoria y arranca de cero en cada arranque.
/// Sin distinguir la corrida, una marca vieja (revision 57) puede coincidir
/// con una revision 57 de OTRA corrida, y el que pregunta concluye que esta al
/// dia justo cuando se perdio todo lo que paso en el medio. Con el `epoch`,
/// una corrida distinta es siempre "no te conozco, veni a comparar".
///
/// Importa mas desde que la marca se guarda en disco: antes vivia en memoria y
/// se descartaba al cerrar la app, asi que el choque era raro; guardada, la
/// marca sobrevive dias y coincidir pasa a ser cuestion de tiempo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mark {
    pub epoch: u64,
    pub rev: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Msg {
    /// Primer mensaje cuando el que llama todavia no esta pareado.
    PairRequest {
        uid: String,
        name: String,
        platform: String,
        /// Solo para vincularse con un dispositivo sin pantalla (el server de
        /// archivo, Fase 6.2): ahi no hay nadie para comparar los seis digitos,
        /// asi que la prueba de que tenes derecho a vincular es un token de su
        /// configuracion. Entre dos dispositivos con pantalla va en `None` y
        /// manda el codigo, como siempre.
        ///
        /// `default` a proposito: un peer con la version anterior no lo manda,
        /// y su PairRequest tiene que seguir entrando.
        #[serde(default)]
        token: Option<String>,
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
    /// `gzip` dice si quien pregunta sabe leer el inventario comprimido.
    ///
    /// La capacidad viaja en el pedido y no en el saludo porque es asunto de
    /// estos dos mensajes y de nadie mas: el que pregunta dice que sabe leer,
    /// el que contesta le da eso. Sin estado de sesion que mantener al dia en
    /// los cuatro lugares que procesan un `Hello`.
    ///
    /// `default` a proposito, y en las dos direcciones. Un peer viejo manda
    /// `{"type":"manifestReq"}` sin el campo y se lee como `false`, asi que
    /// recibe el inventario de siempre. Y al reves, un peer viejo que recibe
    /// el campo de mas lo ignora, porque para el esta variante no tiene campos
    /// y serde saltea lo que no conoce (hay un test que lo fija). Sin esto,
    /// actualizar la PC y no el celular rompia el sync entero — bastante peor
    /// que que sea lento.
    ManifestReq {
        #[serde(default)]
        gzip: bool,
    },
    ManifestData {
        manifest: Box<crate::manifest::Manifest>,
    },
    /// El mismo inventario, comprimido (ver `manifest::squeeze`).
    ///
    /// Va como variante aparte y no como un campo de `ManifestData` para que
    /// un peer viejo, que no la conoce, falle al leerla en vez de creer que
    /// recibio un inventario vacio. Solo se manda a quien dijo en su `Hello`
    /// que sabe leerla.
    ManifestGz { data: Vec<u8> },
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
    // --- Aviso de novedades (Fase 6.9) -----------------------------------
    //
    // El sync lo maneja siempre el que llama: el que atiende responde pedidos
    // y no decide nada. Contra el server de archivo eso deja un agujero — un
    // cambio hecho en el celular llega al server enseguida, pero la PC no
    // tiene forma de enterarse, porque nadie le habla. Estos tres mensajes
    // son la forma de que se entere sin preguntar cada tanto: el que llama
    // deja una conexion abierta y el que atiende contesta cuando pasa algo.
    /// "Avisame cuando cambie tu biblioteca." Despues de mandarlo, el que
    /// llama no manda nada mas: escucha.
    ///
    /// `since` es la ultima revision que el que llama conoce. Es lo que hace
    /// que reconectar sea barato: en un celular la conexion no sobrevive medio
    /// minuto —cambia de wifi a datos, el NAT de la operadora corta lo que
    /// esta callado—, asi que se reconecta todo el tiempo. Sin este numero,
    /// cada reconexion tiene que arrastrar una puesta al dia completa por las
    /// dudas; con el, el que atiende sabe si de verdad te perdiste algo y
    /// contesta `Changed` en el acto, o parkea. `None` = "no se de nada":
    /// parkea desde ahora.
    Watch { since: Option<Mark> },
    /// Hubo novedades. Cierra la espera; lo que sigue es un sync normal, por
    /// su propia conexion. La marca es desde donde seguir esperando despues.
    Changed { mark: Mark },
    /// Latido mientras no pasa nada. Una conexion que se queda callada horas
    /// la corta cualquier cosa en el medio (un proxy, el NAT del router) y
    /// del lado que espera no se nota hasta que hace falta.
    ///
    /// Lleva la marca para que el que espera la mantenga al dia sin preguntar:
    /// si se corta despues de horas parkeado, reconecta preguntando por lo
    /// ultimo y no por lo de ayer.
    Ping { mark: Mark },
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
        tune(&stream);
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
        tune(&stream);
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
            .ok_or_else(|| anyhow!("the peer did not send its static key"))?
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
        // Sin `flush`: sobre un `TcpStream` crudo es un no-op, y tenerlo acá
        // hacía creer que había un buffer nuestro pendiente de vaciar. No lo
        // hay — cada frame ya salió con su `write_all`.
        Ok(())
    }

    pub fn recv_bytes(&mut self) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; 65535];
        let frame = read_frame(&self.stream)?;
        let n = self.noise.read_message(&frame, &mut buf)?;
        if n != 4 {
            return Err(anyhow!("invalid header ({n} bytes)"));
        }
        let total = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if total > MAX_MESSAGE {
            return Err(anyhow!("message too large ({total} bytes)"));
        }
        let mut out = Vec::with_capacity(total);
        while out.len() < total {
            let frame = read_frame(&self.stream)?;
            let n = self.noise.read_message(&frame, &mut buf)?;
            out.extend_from_slice(&buf[..n]);
            if n == 0 {
                return Err(anyhow!("the peer cut the message in half"));
            }
        }
        out.truncate(total);
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Framing en el socket: [u32 largo][bytes]
// ---------------------------------------------------------------------------

/// Cabecera y cuerpo en **una** escritura.
///
/// En dos, los 4 bytes de la cabecera salen como su propio paquete y quedan
/// expuestos a la espera de Nagle. Un track son cientos de frames, así que
/// conviene no pagarlo. Copiar 64 KiB a un buffer cuesta microsegundos.
///
/// Ojo con la tentación de darle el crédito de más: esto NO fue lo que arregló
/// las transferencias de 12 s (eso era `opt-level = 0` sobre ChaCha20-Poly1305,
/// ver el `Cargo.toml`). Con el canal ya rápido la diferencia acá es chica.
fn write_frame(mut stream: &TcpStream, data: &[u8]) -> Result<()> {
    let mut out = Vec::with_capacity(4 + data.len());
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
    stream.write_all(&out)?;
    Ok(())
}

/// Ajustes del socket, iguales para las dos puntas.
///
/// Se aplican antes del handshake a propósito: XX son tres mensajes chicos con
/// ida y vuelta, o sea justo el patrón que Nagle penaliza. Lo mismo los
/// mensajes de control (`BlobReq`, `MetaAck`, `Bye`): cortos y estrictamente
/// secuenciales, cada uno esperando la respuesta del otro antes de seguir.
///
/// Es una mejora de latencia en los mensajes chicos, no de throughput en los
/// archivos: lo de los archivos era el cifrado sin optimizar.
fn tune(stream: &TcpStream) {
    // Que falle no es motivo para tirar la conexión: sin esto anda, sólo que
    // lento.
    if let Err(e) = stream.set_nodelay(true) {
        log::debug!("[wire] could not disable Nagle: {e}");
    }
}

fn read_frame(mut stream: &TcpStream) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    if let Some(what) = looks_like_http(&len) {
        return Err(anyhow!(
            "that port is a web server, not a Sway server (it answered {what})"
        ));
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > 65535 {
        return Err(anyhow!("invalid frame ({len} bytes)"));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

/// Los cuatro primeros bytes, leidos como texto, cuando son el principio de
/// algo de HTTP.
///
/// Apuntarle al puerto equivocado es EL error de este protocolo, y pasa por
/// los dos lados: la app contra un proxy web (que contesta `HTTP/1.1 400`), o
/// un navegador contra el server (que manda `GET `). Los dos casos terminaban
/// en "invalid frame (1213486160 bytes)", que es el largo del mensaje leido de
/// cuatro letras ASCII — un numero que no le dice nada a nadie y menos que
/// menos que el puerto es de un servidor web.
fn looks_like_http(head: &[u8; 4]) -> Option<&'static str> {
    match head {
        b"HTTP" => Some("HTTP"),
        b"GET " => Some("GET"),
        b"POST" => Some("POST"),
        b"HEAD" => Some("HEAD"),
        b"PUT " => Some("PUT"),
        b"OPTI" => Some("OPTIONS"),
        b"DELE" => Some("DELETE"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::{TcpListener, TcpStream};

    /// Apuntarle al puerto de un proxy web es EL error de este protocolo. El
    /// mensaje tiene que decir eso y no un numero de cuatro bytes leidos como
    /// largo.
    #[test]
    fn contra_un_servidor_web_el_error_lo_dice() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let web = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Como un proxy de verdad: primero lee lo que le mandan —nuestro
            // handshake, que para él es basura— y recién ahí contesta. Cerrar
            // antes de leer daría un error de socket y no el que se prueba.
            let mut scratch = [0u8; 512];
            let _ = stream.read(&mut scratch);
            let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
            // Que el cliente alcance a leer la respuesta antes del cierre.
            std::thread::sleep(std::time::Duration::from_millis(200));
        });

        let stream = TcpStream::connect(addr).unwrap();
        let (private, _) = generate_keypair().unwrap();
        // `unwrap_err` pediria que `Session` sea Debug sólo para imprimirlo.
        let msg = match Session::connect(stream, &private) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("no puede haber sesión contra un servidor web"),
        };
        web.join().unwrap();

        assert!(msg.contains("web server"), "mensaje inutil: {msg}");
        assert!(msg.contains("HTTP"), "mensaje inutil: {msg}");
    }

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

    /// Cuánto da el canal sobre loopback, donde no hay red que culpar. Mide
    /// techo real de cifrado + framing. `#[ignore]` porque es una medición, no
    /// una aserción: correr con
    /// `cargo test --release -- --ignored --nocapture channel_throughput`.
    #[test]
    #[ignore]
    fn channel_throughput() {
        const MB: usize = 40;
        let data = vec![7u8; MB * 1024 * 1024];
        let (mut a, mut b) = pair();

        let drain = std::thread::spawn(move || {
            let t = std::time::Instant::now();
            let got = b.recv_bytes().unwrap();
            (got.len(), t.elapsed())
        });

        let t = std::time::Instant::now();
        a.send_bytes(&data).unwrap();
        let send = t.elapsed();
        let (got, recv) = drain.join().unwrap();

        assert_eq!(got, data.len());
        println!(
            "{MB} MB | envio {:?} ({:.1} MB/s) | recepcion {:?} ({:.1} MB/s)",
            send,
            MB as f64 / send.as_secs_f64(),
            recv,
            MB as f64 / recv.as_secs_f64()
        );
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

    /// La compatibilidad hacia atras del inventario comprimido descansa en
    /// esto: que a un peer viejo, para el que `manifestReq` no tiene campos,
    /// el `gzip` de mas no le rompa nada. Si serde lo rechazara, actualizar un
    /// dispositivo y no el otro cortaria el sync entero.
    #[test]
    fn un_campo_de_mas_no_rompe_un_mensaje_sin_campos() {
        let con_sobra = br#"{"type":"bye","gzip":true,"loQueSea":42}"#;
        assert!(
            matches!(serde_json::from_slice::<Msg>(con_sobra), Ok(Msg::Bye)),
            "un peer viejo tiene que poder ignorar lo que no conoce"
        );
    }

    /// Y al reves: el pedido de un peer viejo, sin el campo, se lee como "no
    /// se leer comprimido" — que es la respuesta segura.
    #[test]
    fn un_pedido_sin_el_campo_pide_el_inventario_de_siempre() {
        let viejo = br#"{"type":"manifestReq"}"#;
        match serde_json::from_slice::<Msg>(viejo) {
            Ok(Msg::ManifestReq { gzip }) => assert!(!gzip),
            other => panic!("no se pudo leer el pedido viejo: {other:?}"),
        }
    }

    #[test]
    fn messages_round_trip() {
        let (mut a, mut b) = pair();
        a.send(&Msg::PairRequest {
            uid: "u-1".into(),
            name: "PC".into(),
            platform: "windows".into(),
            token: None,
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
