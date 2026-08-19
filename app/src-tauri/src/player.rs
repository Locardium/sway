// Desktop playback with rodio (symphonia decodes FLAC/MP3/etc).
// The audio stream (cpal) is !Send, so it lives on a dedicated thread that
// receives commands over a channel. Position is published in an AtomicU64.
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

/// Name of the system's default output device, or `None` if there is none
/// (everything disconnected).
fn default_output_name() -> Option<String> {
    use rodio::cpal::traits::{DeviceTrait, HostTrait};
    rodio::cpal::default_host()
        .default_output_device()
        .and_then(|d| d.name().ok())
}

/// How often to check whether the system switched output. It's a query to
/// the audio host, not a free one: one second is imperceptible when
/// switching headphones and costs nothing while nothing changes.
const DEVICE_POLL: Duration = Duration::from_secs(1);

/// Open output: cpal's stream and its handle. The stream **has to stay
/// alive** — if it's dropped, the audio cuts out.
struct Output {
    _stream: OutputStream,
    handle: rodio::OutputStreamHandle,
}

fn open_output() -> Option<Output> {
    match OutputStream::try_default() {
        Ok((stream, handle)) => {
            eprintln!("[player] audio output opened OK");
            Some(Output { _stream: stream, handle })
        }
        Err(e) => {
            eprintln!("[player] could not open audio output: {e}");
            None
        }
    }
}

/// Loads a file into the sink, optionally skipping ahead to `from`.
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

/// The audio stream stays bound to whatever device was the default when it
/// was opened. If the user switches output (plugs in headphones, changes the
/// default in Windows), that stream keeps writing to a device that no longer
/// plays sound: the app looks like it's playing — the bar keeps advancing —
/// and nothing is heard.
///
/// So the output isn't opened just once: what the default is gets watched,
/// and when it changes, it's reopened and the track resumes where it was.
/// And if there was no output at all on startup, it's retried on every Play
/// instead of staying silent forever.
fn run_audio(rx: Receiver<Cmd>, pos_ms: Arc<AtomicU64>) {
    let mut out = open_output();
    let mut sink = out.as_ref().and_then(|o| Sink::try_new(&o.handle).ok());
    let mut vol: f32 = 1.0;
    // What's loaded, so it can be resumed if the output needs to be reopened.
    let mut current: Option<PathBuf> = None;
    let mut device = default_output_name();
    let mut last_check = std::time::Instant::now();

    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Cmd::Play(path)) => {
                // Retry: if there was no output on startup (or it dropped),
                // this is the moment to try again.
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
                // Reflects the new position immediately and skips the store
                // below (get_pos can be slow to reflect the seek => the bar
                // jumping back to the old value).
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

        // Did the system's output change? Reopen and resume where it was.
        if last_check.elapsed() >= DEVICE_POLL {
            last_check = std::time::Instant::now();
            let now_device = default_output_name();
            if now_device != device && now_device.is_some() {
                eprintln!("[player] output changed: {device:?} -> {now_device:?}");
                device = now_device;
                let was_paused = sink.as_ref().map(|s| s.is_paused()).unwrap_or(true);
                let at = Duration::from_millis(pos_ms.load(Ordering::Relaxed));
                if let Some(s) = &sink {
                    s.stop();
                }
                sink = None;
                // The new stream is opened before the old one is released:
                // the assignment drops the previous one only once this one
                // already exists.
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
