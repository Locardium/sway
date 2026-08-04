// Backend de playback para Android/iOS: tauri-plugin-native-audio (Media3/
// ExoPlayer), validado en Fase 0.5 (spike-android-audio/). A diferencia de
// desktop, este plugin se controla directo desde JS — no hay un Player de
// Rust que envolver acá, el propio plugin nativo es el puente.
import { invoke } from '@tauri-apps/api/core';
import { initialize, setSource, play, pause, seekTo as nativeSeekTo, getState } from 'tauri-plugin-native-audio-api';

const getTrackPath = (id: number) => invoke<string>('get_track_path', { id });

let initialized: Promise<unknown> | null = null;
function ensureInitialized() {
  if (!initialized) initialized = initialize();
  return initialized;
}

// file:// + path tal cual (storage interno de la app, sin espacios que
// rompan por falta de encoding en esta fase — ver PRODUCT.md Fase 4).
function toFileUri(path: string) {
  return 'file://' + path.replace(/\\/g, '/');
}

export async function playTrack(id: number) {
  const path = await getTrackPath(id);
  await ensureInitialized();
  await setSource({ src: toFileUri(path), id });
  await play();
}

export async function pausePlayback() {
  await pause();
}

export async function resumePlayback() {
  await play();
}

export async function stopPlayback() {
  await pause();
  await nativeSeekTo(0);
}

export async function seekTo(secs: number) {
  await nativeSeekTo(secs);
}

export async function playbackPosition(): Promise<number> {
  const st = await getState();
  return Math.floor(st.currentTime || 0);
}

// El plugin no expone control de volumen — Android usa los botones de
// hardware. No-op a proposito (ver PlayerBar showVolume en App.tsx).
export async function setVolume(_volume: number) {}
