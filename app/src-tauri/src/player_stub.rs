//! `Player` stub for Android/iOS. Real playback there goes through
//! `tauri-plugin-native-audio`, controlled directly from JS (see
//! `app/src/nativeAudio.ts`) — not through this Rust `Player` (which on
//! desktop drives rodio). The playback commands in `lib.rs` (`play_track`,
//! `pause_playback`, etc.) are unreachable from the frontend on Android
//! (`api.ts` routes to `nativeAudio.ts` instead), but `AppState` still needs
//! them to compile without duplicating `lib.rs` per platform. Same public
//! signature as `player.rs`, with no real behavior.

use std::path::PathBuf;

pub struct Player;

impl Player {
    pub fn new() -> Self {
        Player
    }
    pub fn play(&self, _path: PathBuf) {}
    pub fn pause(&self) {}
    pub fn resume(&self) {}
    pub fn stop(&self) {}
    pub fn seek(&self, _secs: u64) {}
    pub fn set_volume(&self, _vol: f32) {}
    pub fn position_secs(&self) -> u64 {
        0
    }
}
