// Backend de playback para Android/iOS: tauri-plugin-native-audio (Media3/
// ExoPlayer), validado en Fase 0.5 (spike-android-audio/). A diferencia de
// desktop, este plugin se controla directo desde JS — no hay un Player de
// Rust que envolver acá, el propio plugin nativo es el puente.
import { invoke } from '@tauri-apps/api/core';
import {
  initialize,
  setSource,
  play,
  pause,
  seekTo as nativeSeekTo,
  getState,
  addStateListener,
  type NativeAudioState,
} from 'tauri-plugin-native-audio-api';

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
  quiet();
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
  quiet();
  await pause();
  await nativeSeekTo(0);
}

export async function seekTo(secs: number) {
  quiet();
  await nativeSeekTo(secs);
}

export async function playbackPosition(): Promise<number> {
  const st = await getState();
  return Math.floor(st.currentTime || 0);
}

// El plugin no expone control de volumen — Android usa los botones de
// hardware. No-op a proposito (ver PlayerBar showVolume en App.tsx).
export async function setVolume(_volume: number) {}

// --- Estado empujado por el plugin ------------------------------------------
//
// El plugin emite su estado en cada tick (25ms en foreground, 250ms en
// background). Escucharlo en vez de hacer polling desde JS da dos cosas que
// el polling no da: fin de track de verdad (`status: 'ended'`) en vez de
// adivinarlo comparando la posicion contra la duracion, y play/pause tocado
// desde la notificacion o el lockscreen.
//
// Para que esto siga vivo con la app en background, `MainActivity.kt` evita
// que el WebView se pause (ahi esta el porque).

export type PlaybackEvent =
  | { type: 'position'; ms: number }
  | { type: 'playing'; value: boolean }
  | { type: 'ended' };

let endedSent = false;

/// Descarta el estado derivado tras un cambio de posicion hecho a proposito
/// (seek, stop, track nuevo).
function quiet() {
  endedSent = false;
}

/// Traduce el chorro de estados del plugin a eventos con sentido para la app.
/// `emit` recibe posicion como mucho cada `POSITION_MS` (el tick nativo es
/// demasiado rapido para meterlo en estado de React), pero los saltos y el
/// fin de track salen al instante.
const POSITION_MS = 200;
let lastPositionEmit = 0;

let lastReportedPlaying: boolean | null = null;

function handleState(st: NativeAudioState, emit: (e: PlaybackEvent) => void) {
  const now = Date.now();
  const ms = Math.round((st.currentTime || 0) * 1000);

  // Play/pause tambien se toca desde la notificacion y el lockscreen, no solo
  // desde la app: sin esto el boton de la app se queda mostrando el estado
  // que ya no es.
  if (st.status !== 'ended' && st.isPlaying !== lastReportedPlaying) {
    lastReportedPlaying = st.isPlaying;
    emit({ type: 'playing', value: st.isPlaying });
  }

  if (st.status === 'ended') {
    if (!endedSent) {
      endedSent = true;
      emit({ type: 'ended' });
    }
    return;
  }
  endedSent = false;

  if (now - lastPositionEmit >= POSITION_MS) {
    lastPositionEmit = now;
    emit({ type: 'position', ms });
  }
}

export async function subscribePlayback(
  emit: (e: PlaybackEvent) => void,
): Promise<() => void> {
  await ensureInitialized();
  return addStateListener((st) => handleState(st, emit));
}
