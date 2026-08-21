// Desktop playback with rodio (symphonia decodes FLAC/MP3/etc).
// The audio stream (cpal) is !Send, so it lives on a dedicated thread that
// receives commands over a channel. Position is published in an AtomicU64.
//
// The player owns track transitions, not the UI. That's forced by gapless and
// crossfade: both need the next track's audio to start while the current one
// is still going, and a round trip out to JS and back cannot hit that timing.
// So the UI hands the player "what comes next" (`set_next`) and then reads
// back "what is playing now" (`state`) — it no longer drives the change.
use rodio::{Decoder, OutputStream, Sink, Source};
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// A track handed to the player: everything it needs to play it without
/// asking anyone anything.
#[derive(Clone, Debug)]
pub struct Cue {
    pub id: i64,
    pub path: PathBuf,
    /// Linear multiplier for this track (trim + normalization, already
    /// resolved to a number by the caller — see `db::playback_gain_db`).
    pub gain: f32,
    /// Known from the library, not from the decoder: for some formats
    /// `total_duration()` is None, and crossfade needs to know where the end
    /// is before it gets there.
    pub duration_ms: u64,
    /// Silence to skip at the head and cut at the tail, measured by the
    /// analyzer. This is what actually makes gapless gapless: rodio queues
    /// two sources with nothing between them, but the encoder padding and
    /// recorded silence inside the files still play, and that silence is the
    /// gap. Zero when the track hasn't been analyzed — untrimmed playback,
    /// same as before.
    pub lead_ms: u64,
    /// Absolute position where the audio ends. 0 = unknown (not analyzed):
    /// the track plays to its real end, untrimmed.
    pub audio_end_ms: u64,
}

impl Cue {
    /// Where the audible part ends, in file time. Falls back to the track's
    /// stored duration when nothing was measured.
    fn audible_end_ms(&self) -> u64 {
        if self.audio_end_ms > 0 {
            self.audio_end_ms
        } else {
            self.duration_ms
        }
    }

    /// How long the track actually plays for once both edges are trimmed.
    fn audible_len_ms(&self) -> u64 {
        self.audible_end_ms().saturating_sub(self.lead_ms)
    }
}

enum Cmd {
    Play(Cue),
    Pause,
    Resume,
    Stop,
    Seek(u64),
    /// Master volume — the room, applied over every track's own gain.
    Volume(f32),
    /// Trim of the track playing right now, changed live from the player bar.
    Gain(f32),
    /// What to play when the current track ends. `None` = nothing follows
    /// (end of queue, or autoplay off): playback simply stops there.
    Next(Option<Cue>),
    Config { crossfade_secs: f32, gapless: bool },
    /// `None` = follow the system default, including when it changes
    /// mid-track. `Some(name)` = pin to that device and stop following.
    Device(Option<String>),
}

/// What the UI polls for. Read from atomics, so it never waits on the audio
/// thread — which is usually mid-decode.
#[derive(serde::Serialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackState {
    /// Track playing now. `None` = stopped. This is what tells the UI the
    /// player advanced on its own during a gapless or crossfade transition.
    pub track_id: Option<i64>,
    pub pos_ms: u64,
    pub playing: bool,
}

pub struct Player {
    tx: Mutex<Sender<Cmd>>,
    pos_ms: Arc<AtomicU64>,
    /// -1 = nothing playing. An `AtomicI64` rather than a lock so the poll
    /// can't ever be blocked by the audio thread.
    track_id: Arc<AtomicI64>,
    playing: Arc<AtomicBool>,
}

impl Player {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let shared = Shared {
            pos_ms: Arc::new(AtomicU64::new(0)),
            track_id: Arc::new(AtomicI64::new(-1)),
            playing: Arc::new(AtomicBool::new(false)),
        };
        let for_thread = shared.clone();
        thread::spawn(move || run_audio(rx, for_thread));
        Player {
            tx: Mutex::new(tx),
            pos_ms: shared.pos_ms,
            track_id: shared.track_id,
            playing: shared.playing,
        }
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.tx.lock().unwrap().send(cmd);
    }

    pub fn play(&self, cue: Cue) {
        self.pos_ms.store(0, Ordering::Relaxed);
        self.track_id.store(cue.id, Ordering::Relaxed);
        self.playing.store(true, Ordering::Relaxed);
        self.send(Cmd::Play(cue));
    }
    pub fn pause(&self) {
        self.playing.store(false, Ordering::Relaxed);
        self.send(Cmd::Pause);
    }
    pub fn resume(&self) {
        self.playing.store(true, Ordering::Relaxed);
        self.send(Cmd::Resume);
    }
    pub fn stop(&self) {
        self.pos_ms.store(0, Ordering::Relaxed);
        self.track_id.store(-1, Ordering::Relaxed);
        self.playing.store(false, Ordering::Relaxed);
        self.send(Cmd::Stop);
    }
    pub fn seek(&self, secs: u64) {
        self.send(Cmd::Seek(secs));
    }
    pub fn set_volume(&self, vol: f32) {
        self.send(Cmd::Volume(vol.clamp(0.0, 1.0)));
    }
    /// Trim of the track playing right now. Above 1.0 is deliberate — that's
    /// what a mixer's gain knob is for on a quiet track — and clipping past
    /// that point is the same trade a mixer makes.
    pub fn set_gain(&self, gain: f32) {
        self.send(Cmd::Gain(gain.clamp(0.0, 4.0)));
    }
    pub fn set_next(&self, next: Option<Cue>) {
        self.send(Cmd::Next(next));
    }
    pub fn configure(&self, crossfade_secs: f32, gapless: bool) {
        self.send(Cmd::Config {
            crossfade_secs: crossfade_secs.clamp(0.0, 12.0),
            gapless,
        });
    }
    pub fn set_device(&self, name: Option<String>) {
        self.send(Cmd::Device(name));
    }

    pub fn position_secs(&self) -> u64 {
        self.pos_ms.load(Ordering::Relaxed) / 1000
    }

    pub fn state(&self) -> PlaybackState {
        let id = self.track_id.load(Ordering::Relaxed);
        PlaybackState {
            track_id: (id >= 0).then_some(id),
            pos_ms: self.pos_ms.load(Ordering::Relaxed),
            playing: self.playing.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone)]
struct Shared {
    pos_ms: Arc<AtomicU64>,
    track_id: Arc<AtomicI64>,
    playing: Arc<AtomicBool>,
}

/// Output devices that can be chosen in Settings, by name. The system default
/// is not in the list: it's offered separately, because "default" means
/// *follow whatever the system does*, which is not the same as pinning the
/// device that happens to be the default right now.
pub fn output_devices() -> Vec<String> {
    use rodio::cpal::traits::{DeviceTrait, HostTrait};
    let Ok(devices) = rodio::cpal::default_host().output_devices() else {
        return Vec::new();
    };
    let mut names: Vec<String> = devices.filter_map(|d| d.name().ok()).collect();
    names.sort();
    names.dedup();
    names
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

/// Idle tick. Long enough not to spin, short enough that the position bar
/// moves smoothly.
const IDLE_TICK: Duration = Duration::from_millis(200);

/// Tick while a transition is in flight (a track queued behind the current
/// one, or a crossfade ramping). The boundary has to be caught quickly: it's
/// where the gain changes from one track's trim to the next one's, and 200 ms
/// of the wrong gain is audible.
const BUSY_TICK: Duration = Duration::from_millis(20);

/// How close to the end of the audible part counts as "at the end".
///
/// The position is sampled, not continuous, so waiting for it to land exactly
/// on the end would mean waiting for a tick that may fall past it. One tick's
/// worth of slack is inaudible and guarantees the hand-off fires.
const END_MARGIN_MS: u64 = 60;

/// Open output: cpal's stream and its handle. The stream **has to stay
/// alive** — if it's dropped, the audio cuts out.
struct Output {
    _stream: OutputStream,
    handle: rodio::OutputStreamHandle,
}

/// Opens the pinned device, or the system default when nothing is pinned.
///
/// A pinned device that isn't there (interface unplugged, headphones gone)
/// falls back to the default rather than leaving the app silent: the choice
/// stays saved and takes effect again when the device comes back.
fn open_output(pinned: Option<&str>) -> Option<Output> {
    use rodio::cpal::traits::{DeviceTrait, HostTrait};
    if let Some(want) = pinned {
        if let Ok(devices) = rodio::cpal::default_host().output_devices() {
            for d in devices {
                if d.name().ok().as_deref() != Some(want) {
                    continue;
                }
                match OutputStream::try_from_device(&d) {
                    Ok((stream, handle)) => {
                        log::info!("[player] audio output opened on {want:?}");
                        return Some(Output { _stream: stream, handle });
                    }
                    Err(e) => log::warn!("[player] could not open {want:?}: {e}"),
                }
            }
        }
        log::warn!("[player] {pinned:?} unavailable, falling back to system default");
    }
    match OutputStream::try_default() {
        Ok((stream, handle)) => {
            log::info!("[player] audio output opened OK");
            Some(Output { _stream: stream, handle })
        }
        Err(e) => {
            log::warn!("[player] could not open audio output: {e}");
            None
        }
    }
}

/// Loads a track into the sink, trimming its silent edges and optionally
/// starting at `from` (a position in **file** time, the same clock the UI
/// shows).
///
/// The trim is what removes the gap between tracks. It is applied to the
/// source rather than by seeking, because a queued track is never seeked —
/// rodio starts it the instant the previous one runs out, and by then there
/// is nobody left to ask.
/// Returns how far into the file the source was started, or `None` if it
/// could not be loaded. That offset matters: `Sink::get_pos` counts only the
/// samples that pass through it, so a source started past the head reports 0
/// while really being `lead` into the track. The caller adds it back to keep
/// every position the UI sees in file time.
fn load(sink: &Sink, cue: &Cue, from: Duration) -> Option<Duration> {
    let Ok(f) = File::open(&cue.path) else {
        log::warn!("[player] open fail {}", cue.path.display());
        return None;
    };
    let src = match Decoder::new(BufReader::new(f)) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("[player] decode fail {}: {e}", cue.path.display());
            return None;
        }
    };

    // Never trim past the point being resumed from, and never past the end.
    let lead = Duration::from_millis(cue.lead_ms).max(from);
    if cue.audio_end_ms > 0 && cue.audible_len_ms() > 0 {
        let remaining = Duration::from_millis(cue.audible_end_ms()).saturating_sub(lead);
        sink.append(src.skip_duration(lead).take_duration(remaining));
    } else {
        sink.append(src.skip_duration(lead));
    }
    Some(lead)
}

/// A deck on its way out during a crossfade: still audible, ramping to
/// silence. Kept separate from the live sink so the position the UI reads
/// always belongs to the incoming track.
struct Dying {
    sink: Sink,
    started: Instant,
    secs: f32,
    from_vol: f32,
}

/// The audio thread's whole world.
struct Engine {
    out: Option<Output>,
    /// The deck playing `cur`.
    sink: Option<Sink>,
    /// The previous deck, fading out under a crossfade.
    dying: Option<Dying>,
    /// Ramp of the incoming track, `None` once it has reached full level.
    fading_in: Option<(Instant, f32)>,
    master: f32,
    cur: Option<Cue>,
    /// What the UI says comes next. Consumed only when a mode actually wants
    /// it (gapless or crossfade); otherwise it just sits here.
    next: Option<Cue>,
    /// `next` was already appended to `sink` behind the current track and is
    /// waiting for the boundary. Held here because once appended it is out of
    /// `next` but not yet `cur`.
    queued: Option<Cue>,
    crossfade_secs: f32,
    gapless: bool,
    pinned: Option<String>,
    device: Option<String>,
    last_device_check: Instant,
    /// The current track played to its end and nothing followed it. Latched
    /// so the position stops being republished — otherwise the bar would snap
    /// back to zero the moment rodio releases the finished source, and the
    /// track would look like it's about to start rather than done.
    ended: bool,
    /// How far into the file the live source was started, in ms. Added to
    /// `Sink::get_pos` so every position leaving here is in file time — the
    /// same clock the seek bar and the track's duration are in.
    ///
    /// Reset to zero after a seek: rodio's own seek already sets the reported
    /// position to the requested one, so adding the lead again would count it
    /// twice.
    pos_offset_ms: u64,
}

impl Engine {
    /// Level for the live deck: the room times this track's trim.
    fn live_volume(&self) -> f32 {
        self.master * self.cur.as_ref().map(|c| c.gain).unwrap_or(1.0)
    }

    fn apply_volume(&self) {
        if self.fading_in.is_some() {
            return; // the ramp owns the volume until it finishes
        }
        if let Some(s) = &self.sink {
            s.set_volume(self.live_volume());
        }
    }

    /// Drops any transition in flight. Called whenever the user takes over
    /// (play, stop, seek): a crossfade towards a track nobody asked for any
    /// more would keep playing under the new one.
    fn cancel_transition(&mut self) {
        if let Some(d) = self.dying.take() {
            d.sink.stop();
        }
        self.fading_in = None;
        self.queued = None;
    }

    fn start(&mut self, cue: Cue, shared: &Shared) {
        // Retry: if there was no output on startup (or it dropped), this is
        // the moment to try again.
        if self.out.is_none() {
            self.out = open_output(self.pinned.as_deref());
            self.device = default_output_name();
        }
        if self.out.is_none() {
            return;
        }
        self.cancel_transition();
        if let Some(s) = &self.sink {
            s.stop();
        }
        // Re-taken after `cancel_transition`, which needs `&mut self`.
        let Some(o) = self.out.as_ref() else { return };
        self.sink = Sink::try_new(&o.handle).ok();
        let Some(s) = self.sink.as_ref() else { return };
        self.ended = false;
        log::info!("[player] Play: {}", cue.path.display());
        s.set_volume(self.master * cue.gain);
        match load(s, &cue, Duration::ZERO) {
            Some(lead) => {
                s.play();
                self.pos_offset_ms = lead.as_millis() as u64;
                shared.track_id.store(cue.id, Ordering::Relaxed);
                shared.playing.store(true, Ordering::Relaxed);
                self.cur = Some(cue);
            }
            None => {
                self.pos_offset_ms = 0;
                shared.track_id.store(-1, Ordering::Relaxed);
                shared.playing.store(false, Ordering::Relaxed);
                self.cur = None;
            }
        }
    }

    /// Feeds the next track into the same sink so there is no silence at the
    /// boundary — which is the whole of what gapless is. rodio plays a sink's
    /// queue back to back with nothing in between, so this is just an append.
    fn preload_gapless(&mut self) {
        let (Some(s), Some(next)) = (self.sink.as_ref(), self.next.clone()) else {
            return;
        };
        if load(s, &next, Duration::ZERO).is_some() {
            log::info!("[player] gapless: queued {}", next.path.display());
            self.queued = Some(next);
            self.next = None;
        } else {
            self.next = None; // unreadable: don't retry it every tick
        }
    }

    /// The sink's queue went from two sounds to one: the track we appended is
    /// now the one being heard.
    fn advance_gapless(&mut self, shared: &Shared) {
        let Some(now) = self.queued.take() else { return };
        self.ended = false;
        // Whatever the UI last named as the follower belonged to the track
        // that just finished. Keeping it would queue that same track again
        // behind itself; the UI sends the right one as soon as it sees the
        // change.
        self.next = None;
        log::info!("[player] gapless: now playing {}", now.path.display());
        // The queued source was appended already skipped past its own lead,
        // so the position it reports needs that added back.
        self.pos_offset_ms = now.lead_ms;
        shared.pos_ms.store(now.lead_ms, Ordering::Relaxed);
        shared.playing.store(true, Ordering::Relaxed);
        shared.track_id.store(now.id, Ordering::Relaxed);
        self.cur = Some(now);
        self.apply_volume();
    }

    /// Starts the incoming track on a second deck and sends the current one
    /// to silence over `crossfade_secs`.
    ///
    /// The incoming track becomes "now playing" immediately, before the
    /// outgoing one is inaudible. That is deliberate: during a crossfade both
    /// are heard, and the one the room is moving towards is the one the UI
    /// should be naming.
    fn start_crossfade(&mut self, shared: &Shared) {
        let (Some(o), Some(incoming)) = (self.out.as_ref(), self.next.clone()) else {
            return;
        };
        let Ok(new_sink) = Sink::try_new(&o.handle) else { return };
        new_sink.set_volume(0.0);
        let Some(lead) = load(&new_sink, &incoming, Duration::ZERO) else {
            self.next = None;
            return;
        };
        new_sink.play();
        self.pos_offset_ms = lead.as_millis() as u64;
        log::info!(
            "[player] crossfade {}s -> {}",
            self.crossfade_secs,
            incoming.path.display()
        );
        if let Some(old) = self.sink.take() {
            let from_vol = self.live_volume();
            self.dying = Some(Dying {
                sink: old,
                started: Instant::now(),
                secs: self.crossfade_secs,
                from_vol,
            });
        }
        self.sink = Some(new_sink);
        self.ended = false;
        self.fading_in = Some((Instant::now(), self.crossfade_secs));
        shared.track_id.store(incoming.id, Ordering::Relaxed);
        self.cur = Some(incoming);
        self.next = None;
    }

    /// Moves both ramps of a crossfade one tick forward.
    fn step_fades(&mut self) {
        if let Some((started, secs)) = self.fading_in {
            let t = (started.elapsed().as_secs_f32() / secs.max(0.01)).min(1.0);
            if let Some(s) = &self.sink {
                s.set_volume(self.live_volume() * t);
            }
            if t >= 1.0 {
                self.fading_in = None;
                self.apply_volume();
            }
        }
        if let Some(d) = &self.dying {
            let t = (d.started.elapsed().as_secs_f32() / d.secs.max(0.01)).min(1.0);
            d.sink.set_volume(d.from_vol * (1.0 - t));
            if t >= 1.0 {
                if let Some(d) = self.dying.take() {
                    d.sink.stop();
                }
            }
        }
    }

    /// Close enough to the end to hand the next track to the sink.
    ///
    /// A track with no known duration is queued straight away: there is no end
    /// to measure from, and being early costs less than missing the boundary.
    /// Queued as soon as the next track is known, rather than a few seconds
    /// before the end.
    ///
    /// Timing the hand-off off the track's duration was wrong: for a VBR MP3
    /// that duration is a bitrate estimate and can be seconds out, so the
    /// window was missed and the track simply stopped at the end instead of
    /// moving on. Handing it over early depends on nothing but the queue, and
    /// costs one open decoder.
    fn ready_to_preload(&self) -> bool {
        self.crossfade_secs <= 0.0
            && self.gapless
            && self.running()
            && self.next.is_some()
            && self.queued.is_none()
            && self.cur.is_some()
    }

    /// Is the current track close enough to its end to start the crossfade?
    fn ready_to_crossfade(&self, pos_ms: u64) -> bool {
        if self.crossfade_secs <= 0.0 || self.next.is_none() || self.dying.is_some() {
            return false;
        }
        if !self.running() {
            return false;
        }
        let Some(cur) = &self.cur else { return false };
        // A track with no known duration can't be crossfaded — there is no
        // "end" to aim at. It plays out and the gapless path (or the UI)
        // handles the change instead.
        if cur.duration_ms == 0 {
            return false;
        }
        let remaining = cur.audible_end_ms().saturating_sub(pos_ms);
        remaining <= (self.crossfade_secs * 1000.0) as u64
    }

    /// Is audio actually moving right now?
    ///
    /// Every automatic transition hangs off this. Without it, a track that
    /// already finished with autoplay off would start the next one the moment
    /// autoplay came back on — the player would be past the trigger point,
    /// see a `next` appear, and take it, while the UI (which stopped polling
    /// once it was paused) went on showing the old track over the new audio.
    /// Turning a setting on is not a transport command.
    fn running(&self) -> bool {
        !self.ended
            && self
                .sink
                .as_ref()
                .map(|s| !s.is_paused() && !s.empty())
                .unwrap_or(false)
    }
}

fn run_audio(rx: Receiver<Cmd>, shared: Shared) {
    let out = open_output(None);
    let sink = out.as_ref().and_then(|o| Sink::try_new(&o.handle).ok());
    let mut e = Engine {
        out,
        sink,
        dying: None,
        fading_in: None,
        master: 1.0,
        cur: None,
        next: None,
        queued: None,
        crossfade_secs: 0.0,
        gapless: true,
        pinned: None,
        device: default_output_name(),
        last_device_check: Instant::now(),
        ended: false,
        pos_offset_ms: 0,
    };

    loop {
        // A transition in flight needs a fast tick (the gain changes at the
        // boundary, and a fade has to be stepped smoothly); the rest of the
        // time there is nothing to do between commands.
        let busy = e.dying.is_some() || e.fading_in.is_some() || e.queued.is_some();
        let tick = if busy { BUSY_TICK } else { IDLE_TICK };

        match rx.recv_timeout(tick) {
            Ok(Cmd::Play(cue)) => {
                e.next = None;
                e.start(cue, &shared);
            }
            Ok(Cmd::Pause) => {
                shared.playing.store(false, Ordering::Relaxed);
                if let Some(s) = &e.sink {
                    s.pause();
                }
                if let Some(d) = &e.dying {
                    d.sink.pause();
                }
            }
            Ok(Cmd::Resume) => {
                e.ended = false;
                shared.playing.store(true, Ordering::Relaxed);
                if let Some(s) = &e.sink {
                    s.play();
                }
                if let Some(d) = &e.dying {
                    d.sink.play();
                }
            }
            Ok(Cmd::Stop) => {
                e.ended = false;
                e.cancel_transition();
                if let Some(s) = &e.sink {
                    s.stop();
                }
                e.cur = None;
                e.next = None;
                shared.track_id.store(-1, Ordering::Relaxed);
                shared.playing.store(false, Ordering::Relaxed);
                e.sink = e.out.as_ref().and_then(|o| Sink::try_new(&o.handle).ok());
                if let Some(s) = &e.sink {
                    s.set_volume(e.master);
                }
            }
            Ok(Cmd::Seek(secs)) => {
                // Seeking away from the end has to undo a transition that
                // already started, otherwise the next track keeps playing
                // underneath after the user jumped back.
                if e.dying.is_some() || e.queued.is_some() {
                    let back = e.cur.clone();
                    e.cancel_transition();
                    if let Some(cue) = back {
                        e.start(cue, &shared);
                    }
                }
                if let Some(s) = &e.sink {
                    // The seek goes to the underlying decoder in file time,
                    // and rodio then reports positions from there — so the
                    // lead offset must stop being added or it counts twice.
                    if s.try_seek(Duration::from_secs(secs)).is_ok() {
                        e.pos_offset_ms = 0;
                    }
                }
                // Reflects the new position immediately and skips the store
                // below (get_pos can be slow to reflect the seek => the bar
                // jumping back to the old value).
                e.ended = false;
                shared.pos_ms.store(secs * 1000, Ordering::Relaxed);
                continue;
            }
            Ok(Cmd::Volume(v)) => {
                e.master = v;
                e.apply_volume();
            }
            Ok(Cmd::Gain(g)) => {
                if let Some(c) = e.cur.as_mut() {
                    c.gain = g;
                }
                e.apply_volume();
            }
            Ok(Cmd::Next(next)) => {
                // A track already appended to the sink can't be un-appended,
                // so a late change of mind only affects what hasn't been
                // queued yet.
                e.next = next;
            }
            Ok(Cmd::Config { crossfade_secs, gapless }) => {
                e.crossfade_secs = crossfade_secs;
                e.gapless = gapless;
            }
            Ok(Cmd::Device(name)) => {
                if e.pinned != name {
                    e.pinned = name;
                    reopen_on(&mut e, &shared);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        e.step_fades();

        // Where the current track is, in file time. Read before the
        // transition checks: both of them are decisions about how close the
        // end is.
        let pos_ms = e
            .sink
            .as_ref()
            .map(|s| s.get_pos().as_millis() as u64 + e.pos_offset_ms)
            .unwrap_or(0);

        // Is the current track past the point where its audio stops?
        //
        // Every automatic decision below is driven from this rather than from
        // the source ending, because a source ending is not something we can
        // count on: the MP3 decoder does not reliably run out at the end of
        // the file, which is exactly why the old code advanced tracks by
        // comparing the position against the duration. The queue dropping is
        // kept as the fast path; this is the one that always fires.
        let past_end = e
            .cur
            .as_ref()
            .map(|c| {
                let end = c.audible_end_ms();
                end > 0 && pos_ms + END_MARGIN_MS >= end
            })
            .unwrap_or(false);

        let queued_len = e.sink.as_ref().map(|s| s.len()).unwrap_or(0);

        // Past the end with the next track already sitting in the sink behind
        // this one: hand over now instead of waiting for a source that may
        // never report itself finished.
        if e.queued.is_some() && past_end && queued_len > 1 {
            if let Some(s) = &e.sink {
                s.skip_one();
            }
        }

        // The gapless boundary: the sink's queue dropped back to one sound,
        // so what we appended behind the current track is now the audible one.
        if e.queued.is_some() && (queued_len <= 1 || past_end) {
            e.advance_gapless(&shared);
            // Nothing else this round: `past_end` was worked out from the
            // track that just finished, and reusing it against the one that
            // just started would end it before it played a note.
            continue;
        }

        if e.ready_to_crossfade(pos_ms) {
            e.start_crossfade(&shared);
        } else if e.ready_to_preload() {
            e.preload_gapless();
        }

        // Did the system's output change? Reopen and resume where it was.
        // Only while following the default: a pinned device is a choice, and
        // the system swapping its default is not a reason to leave it.
        if e.pinned.is_none() && e.last_device_check.elapsed() >= DEVICE_POLL {
            e.last_device_check = Instant::now();
            let now_device = default_output_name();
            if now_device != e.device && now_device.is_some() {
                log::info!("[player] output changed: {:?} -> {now_device:?}", e.device);
                reopen_on(&mut e, &shared);
            }
        }

        // Natural end of the queue: the track finished and nothing was lined
        // up behind it. Reported as "not playing" rather than as a stop —
        // the track stays loaded and selected, which is what the transport
        // buttons expect to find.
        let idle = e
            .sink
            .as_ref()
            .map(|s| (s.empty() || past_end) && !s.is_paused())
            .unwrap_or(false);
        if !e.ended && idle && e.cur.is_some() && e.queued.is_none() && e.dying.is_none() {
            e.ended = true;
            shared.playing.store(false, Ordering::Relaxed);
            if let Some(cur) = &e.cur {
                // Park the bar at the end of the file, not at the end of the
                // audible part: the trailing silence was skipped, but the
                // track really is over and a bar stopping short of the end
                // would just look stuck.
                shared.pos_ms.store(cur.duration_ms, Ordering::Relaxed);
            }
        }

        if e.sink.is_some() && !e.ended {
            shared.pos_ms.store(pos_ms, Ordering::Relaxed);
        }
    }
}

/// Reopens the output (the system default moved, or the user pinned a
/// different device) and resumes the current track where it was.
///
/// The audio stream stays bound to whatever device was open when it was
/// created. Without this, switching output leaves the app writing to a device
/// that no longer plays: the bar keeps advancing and nothing is heard.
fn reopen_on(e: &mut Engine, shared: &Shared) {
    let was_paused = e.sink.as_ref().map(|s| s.is_paused()).unwrap_or(true);
    let at = Duration::from_millis(shared.pos_ms.load(Ordering::Relaxed));
    // A transition can't survive the move: the two decks belong to the old
    // stream. Whatever was crossfading in is dropped and the current track is
    // what comes back.
    e.cancel_transition();
    if let Some(s) = &e.sink {
        s.stop();
    }
    e.sink = None;
    // The new stream is opened before the old one is released: the assignment
    // drops the previous one only once this one already exists.
    e.out = open_output(e.pinned.as_deref());
    e.device = default_output_name();
    let Some(o) = e.out.as_ref() else { return };
    e.sink = Sink::try_new(&o.handle).ok();
    let (Some(s), Some(cur)) = (e.sink.as_ref(), e.cur.clone()) else {
        return;
    };
    s.set_volume(e.master * cur.gain);
    match load(s, &cur, at) {
        Some(lead) => {
            e.pos_offset_ms = lead.as_millis() as u64;
            if was_paused {
                s.pause();
            } else {
                s.play();
            }
        }
        None => s.pause(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 44_100;

    fn wav(path: &std::path::Path, lead_ms: u64, tone_ms: u64, trail_ms: u64) {
        use std::io::Write;
        let f = |ms: u64| (RATE as u64 * ms / 1000) as usize;
        let mut pcm = vec![0i16; f(lead_ms) * 2];
        for i in 0..f(tone_ms) {
            let t = i as f32 / RATE as f32;
            let v = ((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
            pcm.push(v);
            pcm.push(v);
        }
        pcm.extend(std::iter::repeat(0i16).take(f(trail_ms) * 2));
        let data_len = (pcm.len() * 2) as u32;
        let mut o = File::create(path).unwrap();
        o.write_all(b"RIFF").unwrap();
        o.write_all(&(36 + data_len).to_le_bytes()).unwrap();
        o.write_all(b"WAVEfmt ").unwrap();
        o.write_all(&16u32.to_le_bytes()).unwrap();
        o.write_all(&1u16.to_le_bytes()).unwrap();
        o.write_all(&2u16.to_le_bytes()).unwrap();
        o.write_all(&RATE.to_le_bytes()).unwrap();
        o.write_all(&(RATE * 4).to_le_bytes()).unwrap();
        o.write_all(&4u16.to_le_bytes()).unwrap();
        o.write_all(&16u16.to_le_bytes()).unwrap();
        o.write_all(b"data").unwrap();
        o.write_all(&data_len.to_le_bytes()).unwrap();
        for s in &pcm {
            o.write_all(&s.to_le_bytes()).unwrap();
        }
    }

    fn tmp(n: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("sway-player-{}-{n}.wav", std::process::id()));
        p
    }

    fn cue(path: PathBuf, dur_ms: u64, lead_ms: u64, audio_end_ms: u64) -> Cue {
        Cue { id: 1, path, gain: 1.0, duration_ms: dur_ms, lead_ms, audio_end_ms }
    }

    fn engine_with(sink: Sink) -> Engine {
        Engine {
            out: None,
            sink: Some(sink),
            dying: None,
            fading_in: None,
            master: 1.0,
            cur: None,
            next: None,
            queued: None,
            crossfade_secs: 0.0,
            gapless: true,
            pinned: None,
            device: None,
            last_device_check: Instant::now(),
            ended: false,
            pos_offset_ms: 0,
        }
    }

    /// The engine's own boundary detection, which is what actually advances
    /// the track. Drives the exact sequence the loop performs: play A, take
    /// the queued B, drain, and check the hand-off happens exactly once.
    #[test]
    fn the_engine_hands_over_at_the_boundary() {
        let pa = tmp("ea");
        let pb = tmp("eb");
        wav(&pa, 0, 300, 400);
        wav(&pb, 0, 300, 0);
        let (sink, queue) = Sink::new_idle();
        let shared = Shared {
            pos_ms: Arc::new(AtomicU64::new(0)),
            track_id: Arc::new(AtomicI64::new(-1)),
            playing: Arc::new(AtomicBool::new(true)),
        };

        let mut e = engine_with(sink);
        let ca = Cue { id: 1, path: pa.clone(), gain: 1.0, duration_ms: 700, lead_ms: 0, audio_end_ms: 300 };
        let cb = Cue { id: 2, path: pb.clone(), gain: 1.0, duration_ms: 300, lead_ms: 0, audio_end_ms: 300 };
        load(e.sink.as_ref().unwrap(), &ca, Duration::ZERO).unwrap();
        e.cur = Some(ca);
        shared.track_id.store(1, Ordering::Relaxed);

        // What the UI does: hand over the next track.
        e.next = Some(cb);
        assert!(e.ready_to_preload(), "engine should want to queue the next track");
        e.preload_gapless();
        assert!(e.queued.is_some(), "next track queued");

        // Drain, running the loop's boundary check after every sample.
        let mut it = queue.into_iter();
        let cap = (RATE as usize) * 2 * 3;
        let mut n = 0usize;
        let mut advanced_at = None;
        while n < cap {
            if it.next().is_none() {
                break;
            }
            n += 1;
            if e.queued.is_some() && e.sink.as_ref().map(|s| s.len()).unwrap_or(0) <= 1 {
                e.advance_gapless(&shared);
                advanced_at = Some(n);
            }
        }
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);

        let at = advanced_at.expect("the engine never handed over to the queued track");
        let ms = (at / 2) as u64 * 1000 / RATE as u64;
        assert!((ms as i64 - 300).abs() < 120, "handed over at {ms} ms, expected ~300");
        assert_eq!(shared.track_id.load(Ordering::Relaxed), 2, "now playing the second track");
        assert_eq!(e.cur.as_ref().unwrap().id, 2);
    }

    /// The hand-off must not depend on the source reporting itself finished.
    ///
    /// This is the case that broke playback in the field: the MP3 decoder
    /// does not reliably run out at the end of the file, so `len()` never
    /// dropped, nothing advanced, and the track sat there in silence. Here
    /// the track is deliberately given an audible end far short of the file,
    /// so the queue is still holding two sources when the boundary passes —
    /// exactly the shape of the bug.
    #[test]
    fn the_handover_happens_on_position_even_if_the_source_never_ends() {
        let pa = tmp("pa");
        let pb = tmp("pb");
        wav(&pa, 0, 2000, 0);
        wav(&pb, 0, 300, 0);
        let (sink, queue) = Sink::new_idle();
        let shared = Shared {
            pos_ms: Arc::new(AtomicU64::new(0)),
            track_id: Arc::new(AtomicI64::new(-1)),
            playing: Arc::new(AtomicBool::new(true)),
        };
        let mut e = engine_with(sink);
        // Claims to end at 300 ms while the file really runs 2 s: the source
        // is still going strong when the boundary arrives.
        let ca = Cue { id: 1, path: pa.clone(), gain: 1.0, duration_ms: 2000, lead_ms: 0, audio_end_ms: 0 };
        let cb = Cue { id: 2, path: pb.clone(), gain: 1.0, duration_ms: 300, lead_ms: 0, audio_end_ms: 0 };
        load(e.sink.as_ref().unwrap(), &ca, Duration::ZERO).unwrap();
        let mut ca_short = ca.clone();
        ca_short.audio_end_ms = 300;
        e.cur = Some(ca_short);
        e.next = Some(cb);
        e.preload_gapless();
        assert!(e.queued.is_some());

        let mut it = queue.into_iter();
        let cap = (RATE as usize) * 2 * 3;
        let mut n = 0usize;
        let mut advanced = false;
        while n < cap && !advanced {
            if it.next().is_none() {
                break;
            }
            n += 1;
            let pos_ms = (n / 2) as u64 * 1000 / RATE as u64;
            let past_end = e
                .cur
                .as_ref()
                .map(|c| {
                    let end = c.audible_end_ms();
                    end > 0 && pos_ms + END_MARGIN_MS >= end
                })
                .unwrap_or(false);
            let queued_len = e.sink.as_ref().map(|s| s.len()).unwrap_or(0);
            if e.queued.is_some() && past_end && queued_len > 1 {
                if let Some(s) = &e.sink {
                    s.skip_one();
                }
            }
            if e.queued.is_some() && (queued_len <= 1 || past_end) {
                e.advance_gapless(&shared);
                advanced = true;
            }
        }
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);

        assert!(advanced, "never handed over: the source outlived its audible end");
        assert_eq!(shared.track_id.load(Ordering::Relaxed), 2);
    }

    /// The whole gapless mechanism, driven without an audio device: two
    /// tracks appended to one sink, the queue drained by hand.
    ///
    /// This is the check that matters — `len()` dropping from 2 to 1 is the
    /// ONLY signal the engine has that the boundary happened, so if a trimmed
    /// source fails to end, playback stops dead at the end of a track and
    /// nothing advances.
    #[test]
    fn a_trimmed_source_ends_and_the_queue_moves_on() {
        let a = tmp("a");
        let b = tmp("b");
        // 100 ms of silence, 400 ms of tone, 500 ms of silence.
        wav(&a, 100, 400, 500);
        wav(&b, 0, 300, 0);

        let (sink, queue) = Sink::new_idle();
        let ca = cue(a.clone(), 1000, 100, 500);
        let cb = cue(b.clone(), 300, 0, 300);
        assert!(load(&sink, &ca, Duration::ZERO).is_some());
        assert!(load(&sink, &cb, Duration::ZERO).is_some());
        assert_eq!(sink.len(), 2, "both sources queued");

        // Drain by hand. The queue is built with keep-alive, so it hands
        // out silence forever once empty — the iterator ending is NOT the
        // signal. `len()` is: it drops as each source reports itself done,
        // and that drop is the only thing the engine can see.
        let mut samples = 0usize;
        let mut at_one: Option<usize> = None;
        let mut at_zero: Option<usize> = None;
        let cap = (RATE as usize) * 2 * 3; // 3 s of headroom
        let mut it = queue.into_iter();
        while samples < cap {
            if it.next().is_none() {
                break;
            }
            samples += 1;
            match sink.len() {
                1 if at_one.is_none() => at_one = Some(samples),
                0 if at_zero.is_none() => at_zero = Some(samples),
                _ => {}
            }
        }
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);

        let ms = |n: usize| (n / 2) as u64 * 1000 / RATE as u64;
        let first = at_one.map(ms).expect("the first source never ended");
        let both = at_zero.map(ms).expect("the second source never ended");
        // A is trimmed to its 400 ms of tone, B runs its full 300 ms.
        assert!((first as i64 - 400).abs() < 120, "first ended at {first} ms, expected ~400");
        assert!((both as i64 - 700).abs() < 120, "queue drained at {both} ms, expected ~700");
        assert_eq!(sink.len(), 0, "both sources reported themselves done");
    }
}
