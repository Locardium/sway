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

/// Nombre del dispositivo de salida por defecto del sistema, o `None` si no
/// hay ninguno (todos desconectados).
fn default_output_name() -> Option<String> {
    use rodio::cpal::traits::{DeviceTrait, HostTrait};
    rodio::cpal::default_host()
        .default_output_device()
        .and_then(|d| d.name().ok())
}

/// Cada cuánto se comprueba si el sistema cambió de salida. Es una consulta al
/// host de audio, no algo gratis: un segundo es imperceptible al cambiar de
/// auriculares y no cuesta nada mientras no se toca nada.
const DEVICE_POLL: Duration = Duration::from_secs(1);

/// Salida abierta: el stream de cpal y su handle. El stream **tiene que
/// seguir vivo** — si se dropea, se corta el audio.
struct Output {
    _stream: OutputStream,
    handle: rodio::OutputStreamHandle,
}

fn open_output() -> Option<Output> {
    match OutputStream::try_default() {
        Ok((stream, handle)) => {
            eprintln!("[player] salida de audio abierta OK");
            Some(Output { _stream: stream, handle })
        }
        Err(e) => {
            eprintln!("[player] no se pudo abrir salida de audio: {e}");
            None
        }
    }
}

/// Carga un archivo en el sink, opcionalmente salteando a `from`.
fn load(sink: &Sink, path: &std::path::Path, from: Duration) -> bool {
    let Ok(f) = File::open(path) else {
        eprintln!("[player] open fail {}", path.display());
        return false;
    };
    match Decoder::new(BufReader::new(f)) {
        Ok(src) => {
            sink.append(src);
            if !from.is_zero() {
                let _ = sink.try_seek(from);
            }
            true
        }
        Err(e) => {
            eprintln!("[player] decode fail {}: {e}", path.display());
            false
        }
    }
}

/// El stream de audio queda atado al dispositivo que era el default cuando se
/// abrió. Si el usuario cambia de salida (enchufa auriculares, cambia el
/// default en Windows), ese stream sigue escribiendo a un dispositivo que ya
/// no suena: la app parece reproducir —la barra avanza— y no se escucha nada.
///
/// Por eso la salida no se abre una sola vez: se vigila cuál es el default y,
/// cuando cambia, se reabre y se retoma el track donde estaba. Y si al
/// arrancar no había ninguna salida, se reintenta en cada Play en vez de
/// quedarse mudo para siempre.
fn run_audio(rx: Receiver<Cmd>, pos_ms: Arc<AtomicU64>) {
    let mut out = open_output();
    let mut sink = out.as_ref().and_then(|o| Sink::try_new(&o.handle).ok());
    let mut vol: f32 = 1.0;
    // Qué está cargado, para poder retomarlo si hay que reabrir la salida.
    let mut current: Option<PathBuf> = None;
    let mut device = default_output_name();
    let mut last_check = std::time::Instant::now();

    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Cmd::Play(path)) => {
                // Reintento: si al arrancar no había salida (o se cayó), este
                // es el momento de volver a intentar.
                if out.is_none() {
                    out = open_output();
                    device = default_output_name();
                }
                let Some(o) = out.as_ref() else { continue };
                if let Some(s) = &sink {
                    s.stop();
                }
                sink = Sink::try_new(&o.handle).ok();
                let Some(s) = sink.as_ref() else { continue };
                eprintln!("[player] Play: {}", path.display());
                s.set_volume(vol);
                if load(s, &path, Duration::ZERO) {
                    s.play();
                    current = Some(path);
                } else {
                    current = None;
                }
            }
            Ok(Cmd::Pause) => {
                if let Some(s) = &sink {
                    s.pause();
                }
            }
            Ok(Cmd::Resume) => {
                if let Some(s) = &sink {
                    s.play();
                }
            }
            Ok(Cmd::Stop) => {
                if let Some(s) = &sink {
                    s.stop();
                }
                current = None;
                sink = out.as_ref().and_then(|o| Sink::try_new(&o.handle).ok());
                if let Some(s) = &sink {
                    s.set_volume(vol);
                }
            }
            Ok(Cmd::Seek(secs)) => {
                if let Some(s) = &sink {
                    let _ = s.try_seek(Duration::from_secs(secs));
                }
                // Refleja la posicion nueva de inmediato y salta el store de
                // abajo (get_pos puede tardar en reflejar el seek => barra
                // saltando al valor viejo).
                pos_ms.store(secs * 1000, Ordering::Relaxed);
                continue;
            }
            Ok(Cmd::Volume(v)) => {
                vol = v;
                if let Some(s) = &sink {
                    s.set_volume(v);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // ¿Cambió la salida del sistema? Reabrir y retomar donde estaba.
        if last_check.elapsed() >= DEVICE_POLL {
            last_check = std::time::Instant::now();
            let now_device = default_output_name();
            if now_device != device && now_device.is_some() {
                eprintln!("[player] salida cambió: {device:?} -> {now_device:?}");
                device = now_device;
                let was_paused = sink.as_ref().map(|s| s.is_paused()).unwrap_or(true);
                let at = Duration::from_millis(pos_ms.load(Ordering::Relaxed));
                if let Some(s) = &sink {
                    s.stop();
                }
                sink = None;
                // El stream nuevo se abre antes de soltar el viejo: la
                // asignación dropea el anterior recién cuando este ya existe.
                out = open_output();
                if let Some(o) = out.as_ref() {
                    sink = Sink::try_new(&o.handle).ok();
                    if let (Some(s), Some(path)) = (sink.as_ref(), current.as_ref()) {
                        s.set_volume(vol);
                        if load(s, path, at) && !was_paused {
                            s.play();
                        } else {
                            s.pause();
                        }
                    }
                }
            }
        }

        if let Some(s) = &sink {
            pos_ms.store(s.get_pos().as_millis() as u64, Ordering::Relaxed);
        }
    }
}
