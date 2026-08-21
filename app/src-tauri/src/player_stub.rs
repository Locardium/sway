//! `Player` stub for Android/iOS. Real playback there goes through the
//! vendored native-audio plugin (`crates/native-audio`), controlled directly
//! from JS (see `app/src/nativeAudio.ts`) — not through this Rust `Player`
//! (which on desktop drives rodio). The playback commands in `lib.rs`
//! (`play_track`, `pause_playback`, etc.) are unreachable from the frontend on
//! Android (`api.ts` routes to `nativeAudio.ts` instead), but `AppState` still
//! needs them to compile without duplicating `lib.rs` per platform. Same
//! public signature as `player.rs`, with no real behavior.
//!
//! Volume, gain, gapless, crossfade and output device all work on the phone —
//! the fork added them (see `crates/native-audio/FORK.md`). They are still
//! no-ops *here* because none of it is on this side of the wire: the plugin is
//! the player, and JS is what talks to it.

use std::path::PathBuf;

#[derive(Clone, Debug)]
#[allow(dead_code)] // mirrors player.rs's Cue so lib.rs::cue_for compiles identically on both platforms; fields unused here since play() ignores the cue
pub struct Cue {
    pub id: i64,
    pub path: PathBuf,
    pub gain: f32,
    pub duration_ms: u64,
    /// Unused here — nothing on this side trims. They exist so the `Cue` that
    /// `lib.rs::cue_for` builds compiles the same on both platforms: that
    /// function is shared, and the alternative is a `#[cfg]` around the two
    /// fields it fills in.
    ///
    /// The plugin does the equivalent work from its own end: it gets the level
    /// through `track_cue` (same `db::playback_gain_db` this `gain` comes
    /// from) and the seamless handover from ExoPlayer's playlist, so nothing
    /// is missing on the phone — it just doesn't arrive through this struct.
    pub lead_ms: u64,
    pub audio_end_ms: u64,
}

#[derive(serde::Serialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackState {
    pub track_id: Option<i64>,
    pub pos_ms: u64,
    pub playing: bool,
}

pub struct Player;

/// No device to choose from: Media3 plays wherever Android routes it.
pub fn output_devices() -> Vec<String> {
    Vec::new()
}

impl Player {
    pub fn new() -> Self {
        Player
    }
    pub fn play(&self, _cue: Cue) {}
    pub fn pause(&self) {}
    pub fn resume(&self) {}
    pub fn stop(&self) {}
    pub fn seek(&self, _secs: u64) {}
    pub fn set_volume(&self, _vol: f32) {}
    pub fn set_gain(&self, _gain: f32) {}
    pub fn set_next(&self, _next: Option<Cue>) {}
    pub fn configure(&self, _crossfade_secs: f32, _gapless: bool) {}
    pub fn set_device(&self, _name: Option<String>) {}
    pub fn position_secs(&self) -> u64 {
        0
    }
    pub fn state(&self) -> PlaybackState {
        PlaybackState { track_id: None, pos_ms: 0, playing: false }
    }
}
