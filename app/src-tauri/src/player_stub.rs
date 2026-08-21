//! `Player` stub for Android/iOS. Real playback there goes through
//! `tauri-plugin-native-audio`, controlled directly from JS (see
//! `app/src/nativeAudio.ts`) — not through this Rust `Player` (which on
//! desktop drives rodio). The playback commands in `lib.rs` (`play_track`,
//! `pause_playback`, etc.) are unreachable from the frontend on Android
//! (`api.ts` routes to `nativeAudio.ts` instead), but `AppState` still needs
//! them to compile without duplicating `lib.rs` per platform. Same public
//! signature as `player.rs`, with no real behavior.
//!
//! The gain/gapless/crossfade/device methods are stubs here for a second
//! reason: that plugin's API is `setSource/play/pause/seekTo/setRate` and
//! nothing else — no volume, no queue, no device selection. Making any of
//! those work on the phone means forking the plugin (Kotlin + Media3), which
//! is its own piece of work.

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
