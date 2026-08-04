//! Stub de `Player` para Android/iOS. La reproduccion real ahi va por
//! `tauri-plugin-native-audio`, controlado directo desde JS (ver
//! `app/src/nativeAudio.ts>`) — no por este `Player` de Rust (que en desktop
//! mueve rodio). Los comandos de playback en `lib.rs` (`play_track`,
//! `pause_playback`, etc.) son inalcanzables desde el frontend en Android
//! (`api.ts` enruta a `nativeAudio.ts` en su lugar), pero `AppState` los
//! sigue necesitando para compilar sin duplicar `lib.rs` por plataforma.
//! Misma firma publica que `player.rs`, sin comportamiento real.

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
