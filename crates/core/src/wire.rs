//! Encrypted channel between two Sway devices (Phase 5.2).
//!
//! **Noise XX** handshake (`Noise_XX_25519_ChaChaPoly_BLAKE2s`) over TCP.
//! XX exchanges the static keys of both sides, so after the handshake each
//! one knows which public key it's talking to — but not yet whether that key
//! belongs to who it claims to be. That's resolved by the verification code
//! (see `sas_code`), not by the cryptography.
//!
//! Noise was chosen over TLS because there's no PKI to set up: each device
//! just has a keypair and nothing else. No certificates to generate, no
//! chains to validate, no trust store to maintain on Android.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use snow::{Builder, HandshakeState, TransportState};
use std::io::{Read, Write};
use std::net::TcpStream;

const PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";

/// Noise's cap for a single message (64 KiB), minus the 16-byte tag.
const MAX_NOISE_PAYLOAD: usize = 65535 - 16;
/// Sanity bound on an incoming logical message: keeps a hostile peer (or a
/// bug) from making us allocate unbounded memory.
const MAX_MESSAGE: usize = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// How up to date one device is on the other's library.
///
/// `epoch` identifies the run of the side being asked, and it's not
/// decoration: the revision count lives in memory and starts from zero on
/// every startup. Without distinguishing the run, an old mark (revision 57)
/// could match a revision 57 from ANOTHER run, and the one asking would
/// conclude it's up to date exactly when it lost everything that happened in
/// between. With `epoch`, a different run always means "I don't know you,
/// come compare".
///
/// This matters more now that the mark is saved to disk: it used to live in
/// memory and get discarded when the app closed, so the collision was rare;
/// saved, the mark survives for days and colliding becomes a matter of time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mark {
    pub epoch: u64,
    pub rev: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Msg {
    /// First message when the caller isn't paired yet.
    PairRequest {
        uid: String,
        name: String,
        platform: String,
        /// Only for pairing with a screenless device (the file server, Phase
        /// 6.2): there's no one there to compare the six digits, so the proof
        /// that you have the right to pair is a token from its config.
        /// Between two devices with a screen this is `None` and the code is
        /// sent, as usual.
        ///
        /// `default` on purpose: a peer on the previous version doesn't send
        /// it, and its PairRequest still has to be accepted.
        #[serde(default)]
        token: Option<String>,
    },
    /// Decision from the receiving side.
    PairResponse { accepted: bool },
    /// Decision from the calling side. Pairing only goes through if BOTH
    /// accepted: it's enough for one to see a different code to abort.
    PairAck { accepted: bool },
    /// Introduction between devices already paired.
    Hello {
        uid: String,
        name: String,
        platform: String,
        tracks: i64,
        playlists: i64,
        clock_ms: i64,
    },
    /// Orderly disconnect with a reason, so it can be shown in the UI on the
    /// other side.
    Reject { reason: String },
    /// "I removed you from my devices". Pairing is saved on both sides, so
    /// unpairing has to notify the other or it keeps believing they're paired
    /// forever.
    Unpair { uid: String },
    /// "I don't have you in my list." Sent by whoever receives a `Hello` from
    /// a peer it doesn't know; the caller uses it to realize it was unpaired
    /// from the other side.
    NotPaired,
    /// Request and response for the library inventory (Phase 5.3).
    /// `gzip` says whether the requester knows how to read a compressed
    /// inventory.
    ///
    /// The capability travels in the request and not in the greeting because
    /// it's a matter between these two messages and no one else: the
    /// requester says it can read it, the responder gives it that. No session
    /// state to keep up to date in the four places that process a `Hello`.
    ///
    /// `default` on purpose, in both directions. An old peer sends
    /// `{"type":"manifestReq"}` without the field and it reads as `false`, so
    /// it gets the usual inventory. And conversely, an old peer that receives
    /// the extra field ignores it, because for it this variant has no fields
    /// and serde skips what it doesn't know (there's a test that pins this
    /// down). Without this, updating the PC and not the phone would break
    /// the whole sync — quite a bit worse than it being slow.
    ManifestReq {
        #[serde(default)]
        gzip: bool,
    },
    ManifestData {
        manifest: Box<crate::manifest::Manifest>,
    },
    /// The same inventory, compressed (see `manifest::squeeze`).
    ///
    /// It's a separate variant and not a field of `ManifestData` so that an
    /// old peer, which doesn't know it, fails to read it instead of thinking
    /// it received an empty inventory. Only sent to whoever said in its
    /// `Hello` that it can read it.
    ManifestGz { data: Vec<u8> },
    // --- File transfer (Phase 5.4) -----------------------------------------
    //
    // The bytes do NOT travel inside these messages: they go as raw payloads
    // over the same channel (`Session::send_bytes`). Putting them in the JSON
    // would cost 1.33x in base64 or 3-4x as a number array, over gigabytes.
    // The protocol is: BlobStart -> N raw payloads -> BlobEnd.
    /// Request for a file by its hash. `offset` != 0 is a resume.
    BlobReq { hash: String, offset: u64 },
    /// Starts the transfer: `size` is what's left to send from the offset.
    BlobStart { size: u64 },
    /// End of the transfer; `hash` is that of the COMPLETE file, to verify.
    BlobEnd { hash: String },
    /// Push in the other direction: "take this file". Followed by the raw
    /// payloads and a BlobEnd, same as above.
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
    /// The other side can't serve what was requested (doesn't have it, can't
    /// read it).
    BlobError { reason: String },
    /// Metadata, playlists, folders and memberships the other side is
    /// missing (Phase 5.5). Sent after the files: a membership for a track
    /// that hasn't arrived yet is ignored, so it's better if it already
    /// exists first.
    MetaPush {
        changes: Box<crate::merge::Changes>,
    },
    /// How many records were actually applied on the other side.
    MetaAck {
        applied: crate::merge::Applied,
    },
    // --- Change notification (Phase 6.9) ------------------------------------
    //
    // Sync is always driven by the caller: the responder answers requests and
    // decides nothing. Against the file server that leaves a gap — a change
    // made on the phone reaches the server right away, but the PC has no way
    // to find out, because nobody tells it. These three messages are how it
    // finds out without polling every so often: the caller leaves a
    // connection open and the responder replies when something happens.
    /// "Tell me when your library changes." After sending it, the caller
    /// sends nothing else: it listens.
    ///
    /// `since` is the last revision the caller knows about. It's what makes
    /// reconnecting cheap: on a phone the connection doesn't survive half a
    /// minute —it switches from wifi to mobile data, the carrier's NAT cuts
    /// off anything that's quiet—, so it reconnects constantly. Without this
    /// number, every reconnection would have to drag along a full refresh
    /// just in case; with it, the responder knows whether something was
    /// actually missed and replies `Changed` on the spot, or parks. `None` =
    /// "I don't know anything": park from now on.
    Watch { since: Option<Mark> },
    /// There were changes. Ends the wait; what follows is a normal sync, on
    /// its own connection. The mark is where to keep waiting from afterward.
    Changed { mark: Mark },
    /// Heartbeat while nothing is happening. A connection that stays quiet
    /// for hours gets cut by anything in between (a proxy, the router's NAT)
    /// and the waiting side doesn't notice until it needs to.
    ///
    /// Carries the mark so the waiting side keeps it up to date without
    /// asking: if it gets cut after hours parked, it reconnects asking for
    /// the latest and not for yesterday's.
    Ping { mark: Mark },
    /// Orderly close of the session.
    Bye,
}

// ---------------------------------------------------------------------------
// Verification code
// ---------------------------------------------------------------------------

/// Six digits derived from the handshake hash.
///
/// That hash depends on the ephemeral and static keys of both sides, so a
/// man-in-the-middle ends up with TWO different sessions and two different
/// hashes: it can't make the two codes match. Having the user compare the
/// numbers on the two screens is what turns "there's an encrypted channel
/// with someone" into "there's an encrypted channel with this device".
pub fn sas_code(handshake_hash: &[u8]) -> String {
    let mut n: u32 = 0;
    for b in handshake_hash.iter().take(4) {
        n = (n << 8) | *b as u32;
    }
    format!("{:06}", n % 1_000_000)
}

// ---------------------------------------------------------------------------
// Static keys
// ---------------------------------------------------------------------------

/// Generates a new keypair (once per device).
pub fn generate_keypair() -> Result<(Vec<u8>, Vec<u8>)> {
    let kp = Builder::new(PARAMS.parse()?).generate_keypair()?;
    Ok((kp.private, kp.public))
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

pub struct Session {
    stream: TcpStream,
    noise: TransportState,
    /// The other side's static public key, learned during the handshake.
    pub peer_pubkey: Vec<u8>,
    /// Code to show on screen. Both sides compute the same one.
    pub code: String,
}

impl Session {
    /// Calling side.
    pub fn connect(stream: TcpStream, private_key: &[u8]) -> Result<Self> {
        tune(&stream);
        let mut hs = Builder::new(PARAMS.parse()?)
            .local_private_key(private_key)?
            .build_initiator()?;
        let mut buf = vec![0u8; 65535];

        // XX is three messages: -> e | <- e ee s es | -> s se
        let n = hs.write_message(&[], &mut buf)?;
        write_frame(&stream, &buf[..n])?;
        let msg = read_frame(&stream)?;
        hs.read_message(&msg, &mut buf)?;
        let n = hs.write_message(&[], &mut buf)?;
        write_frame(&stream, &buf[..n])?;

        Self::finish(stream, hs)
    }

    /// Accepting side.
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
        // The handshake hash has to be read BEFORE moving to transport mode:
        // `into_transport_mode` consumes the state.
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

    /// A logical message goes as a header frame with the total length, then
    /// the data frames. Noise can't encrypt more than 64 KiB at once, and
    /// manifests (5.3) and audio chunks (5.4) go well past that size, so the
    /// chunking is handled here, in one place.
    pub fn send_bytes(&mut self, data: &[u8]) -> Result<()> {
        let mut buf = vec![0u8; 65535];
        let header = (data.len() as u32).to_be_bytes();
        let n = self.noise.write_message(&header, &mut buf)?;
        write_frame(&self.stream, &buf[..n])?;
        for chunk in data.chunks(MAX_NOISE_PAYLOAD) {
            let n = self.noise.write_message(chunk, &mut buf)?;
            write_frame(&self.stream, &buf[..n])?;
        }
        // No `flush`: on a raw `TcpStream` it's a no-op, and having it here
        // suggested there was a buffer of ours pending to be flushed. There
        // isn't — every frame already went out with its own `write_all`.
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
// Socket framing: [u32 length][bytes]
// ---------------------------------------------------------------------------

/// Header and body in **one** write.
///
/// In two, the header's 4 bytes go out as their own packet and end up
/// exposed to Nagle's delay. A track is hundreds of frames, so it's worth not
/// paying for that. Copying 64 KiB into a buffer costs microseconds.
///
/// Watch out for the temptation to give this too much credit: this is NOT
/// what fixed the 12s transfers (that was `opt-level = 0` over
/// ChaCha20-Poly1305, see `Cargo.toml`). With the channel already fast, the
/// difference here is small.
fn write_frame(mut stream: &TcpStream, data: &[u8]) -> Result<()> {
    let mut out = Vec::with_capacity(4 + data.len());
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
    stream.write_all(&out)?;
    Ok(())
}

/// Socket settings, the same on both ends.
///
/// Applied before the handshake on purpose: XX is three small back-and-forth
/// messages, exactly the pattern Nagle penalizes. Same for control messages
/// (`BlobReq`, `MetaAck`, `Bye`): short and strictly sequential, each one
/// waiting for the other's reply before continuing.
///
/// This is a latency improvement for small messages, not throughput for
/// files: the file bottleneck was the unoptimized encryption.
fn tune(stream: &TcpStream) {
    // Failing here isn't a reason to drop the connection: without this it
    // still works, just slower.
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

/// The first four bytes, read as text, when they're the start of something
/// HTTP.
///
/// Pointing at the wrong port is THE error of this protocol, and it happens
/// on both sides: the app against a web proxy (which answers `HTTP/1.1 400`),
/// or a browser against the server (which sends `GET `). Both cases used to
/// end up as "invalid frame (1213486160 bytes)", which is the length of the
/// four-letter ASCII message read as a number — a number that means nothing
/// to anyone, let alone that the port belongs to a web server.
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

    /// Pointing at a web proxy's port is THE error of this protocol. The
    /// message has to say that, not a four-byte number read as a length.
    #[test]
    fn against_a_web_server_the_error_says_so() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let web = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Like a real proxy: first it reads what's sent to it —our
            // handshake, which is garbage to it— and only then replies.
            // Closing before reading would give a socket error and not the
            // one being tested.
            let mut scratch = [0u8; 512];
            let _ = stream.read(&mut scratch);
            let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
            // Let the client get a chance to read the response before close.
            std::thread::sleep(std::time::Duration::from_millis(200));
        });

        let stream = TcpStream::connect(addr).unwrap();
        let (private, _) = generate_keypair().unwrap();
        // `unwrap_err` would require `Session` to be Debug just to print it.
        let msg = match Session::connect(stream, &private) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("there can't be a session against a web server"),
        };
        web.join().unwrap();

        assert!(msg.contains("web server"), "useless message: {msg}");
        assert!(msg.contains("HTTP"), "useless message: {msg}");
    }

    /// Sets up both ends of a session over loopback.
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

    /// What the channel gives over loopback, where there's no network to
    /// blame. Measures the real ceiling of encryption + framing. `#[ignore]`
    /// because it's a measurement, not an assertion: run with
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
            "{MB} MB | send {:?} ({:.1} MB/s) | receive {:?} ({:.1} MB/s)",
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
        // Each side learned the other's static key, which is what later gets
        // pinned in `devices`.
        assert_ne!(a.peer_pubkey, b.peer_pubkey);
        assert_eq!(a.peer_pubkey.len(), 32);
    }

    /// Two different sessions have to produce different codes: if the code
    /// didn't depend on the ephemeral keys, a man-in-the-middle could
    /// replicate the same number on both screens.
    #[test]
    fn code_differs_between_sessions() {
        let (a1, _) = pair();
        let (a2, _) = pair();
        assert_ne!(a1.code, a2.code);
    }

    /// Backward compatibility for the compressed inventory rests on this:
    /// that for an old peer, for which `manifestReq` has no fields, the extra
    /// `gzip` doesn't break anything. If serde rejected it, updating one
    /// device and not the other would break the whole sync.
    #[test]
    fn an_extra_field_does_not_break_a_message_with_no_fields() {
        let with_extra = br#"{"type":"bye","gzip":true,"whatever":42}"#;
        assert!(
            matches!(serde_json::from_slice::<Msg>(with_extra), Ok(Msg::Bye)),
            "an old peer has to be able to ignore what it doesn't know"
        );
    }

    /// And the other way around: an old peer's request, without the field,
    /// reads as "I can't read compressed" — which is the safe answer.
    #[test]
    fn a_request_without_the_field_asks_for_the_usual_inventory() {
        let old = br#"{"type":"manifestReq"}"#;
        match serde_json::from_slice::<Msg>(old) {
            Ok(Msg::ManifestReq { gzip }) => assert!(!gzip),
            other => panic!("could not read the old request: {other:?}"),
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
            other => panic!("unexpected message: {other:?}"),
        }
        b.send(&Msg::PairResponse { accepted: true }).unwrap();
        match a.recv().unwrap() {
            Msg::PairResponse { accepted } => assert!(accepted),
            other => panic!("unexpected message: {other:?}"),
        }
    }

    /// A manifest (5.3) or an audio chunk (5.4) go past the 64 KiB Noise
    /// encrypts at once: the chunking has to be transparent.
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
