// Reproduccion desktop con rodio (symphonia decodifica FLAC/MP3/etc).
// El stream de audio (cpal) es !Send, asi que vive en un thread dedicado que
// recibe comandos por un canal. La posicion se publica en un AtomicU64.
use rodio::{Decoder, OutputStream, Sink};
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

enum Cmd {
    Play(PathBuf),
    Pause,
    Resume,
    Stop,
    Seek(u64),
    Volume(f32),
}

pub struct Player {
    tx: Mutex<Sender<Cmd>>,
    pos_ms: Arc<AtomicU64>,
}

impl Player {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let pos_ms = Arc::new(AtomicU64::new(0));
        let pc = pos_ms.clone();
        thread::spawn(move || run_audio(rx, pc));
        Player {
            tx: Mutex::new(tx),
            pos_ms,
        }
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.tx.lock().unwrap().send(cmd);
    }

    pub fn play(&self, path: PathBuf) {
        self.pos_ms.store(0, Ordering::Relaxed);
        self.send(Cmd::Play(path));
    }
    pub fn pause(&self) {
        self.send(Cmd::Pause);
    }
    pub fn resume(&self) {
        self.send(Cmd::Resume);
    }
    pub fn stop(&self) {
        self.pos_ms.store(0, Ordering::Relaxed);
        self.send(Cmd::Stop);
    }
    pub fn seek(&self, secs: u64) {
        self.send(Cmd::Seek(secs));
    }
    pub fn set_volume(&self, vol: f32) {
        self.send(Cmd::Volume(vol.clamp(0.0, 1.0)));
    }
    pub fn position_secs(&self) -> u64 {
        self.pos_ms.load(Ordering::Relaxed) / 1000
    }
}

fn run_audio(rx: Receiver<Cmd>, pos_ms: Arc<AtomicU64>) {
    let (_stream, handle) = match OutputStream::try_default() {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[player] no se pudo abrir salida de audio: {e}");
            return;
        }
    };
    eprintln!("[player] salida de audio abierta OK");
    let mut sink = Sink::try_new(&handle).expect("sink");
    let mut vol: f32 = 1.0;

    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Cmd::Play(path)) => {
                sink.stop();
                sink = Sink::try_new(&handle).expect("sink");
                eprintln!("[player] Play: {}", path.display());
                match File::open(&path) {
                    Ok(f) => match Decoder::new(BufReader::new(f)) {
                        Ok(src) => {
                            sink.set_volume(vol);
                            sink.append(src);
                            sink.play();
                            eprintln!(
                                "[player] reproduciendo: len={} vol={} paused={}",
                                sink.len(),
                                sink.volume(),
                                sink.is_paused()
                            );
                        }
                        Err(e) => eprintln!("[player] decode fail {}: {e}", path.display()),
                    },
                    Err(e) => eprintln!("[player] open fail {}: {e}", path.display()),
                }
            }
            Ok(Cmd::Pause) => sink.pause(),
            Ok(Cmd::Resume) => sink.play(),
            Ok(Cmd::Stop) => {
                sink.stop();
                sink = Sink::try_new(&handle).expect("sink");
                sink.set_volume(vol);
            }
            Ok(Cmd::Seek(secs)) => {
                let _ = sink.try_seek(Duration::from_secs(secs));
            }
            Ok(Cmd::Volume(v)) => {
                vol = v;
                sink.set_volume(v);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        pos_ms.store(sink.get_pos().as_millis() as u64, Ordering::Relaxed);
    }
}
