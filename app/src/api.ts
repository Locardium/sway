import { invoke } from '@tauri-apps/api/core';

export interface Track {
  id: number;
  path: string;
  title: string;
  artist: string;
  album: string;
  genre: string;
  durationMs: number;
  bpm: number | null;
}

// Import: escanea una carpeta, lee tags e inserta tracks. Devuelve cuantos.
export const importFolder = (folder: string) => invoke<number>('import_folder', { folder });
export const listTracks = () => invoke<Track[]>('list_tracks');

// Playback (rodio/symphonia en Rust).
export const playTrack = (id: number) => invoke<void>('play_track', { id });
export const pausePlayback = () => invoke<void>('pause_playback');
export const resumePlayback = () => invoke<void>('resume_playback');
export const stopPlayback = () => invoke<void>('stop_playback');
export const seekTo = (secs: number) => invoke<void>('seek_to', { secs });
export const playbackPosition = () => invoke<number>('playback_position');
