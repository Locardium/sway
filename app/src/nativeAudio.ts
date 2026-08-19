// Playback backend for Android/iOS: tauri-plugin-native-audio (Media3/
// ExoPlayer), validated in Phase 0.5 (spike-android-audio/). Unlike desktop,
// this plugin is controlled directly from JS — there's no Rust Player to
// wrap here, the native plugin itself is the bridge.
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

// file:// + path as-is (app's internal storage, no spaces to break things
// from missing encoding at this phase — see PRODUCT.md Phase 4).
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

// The plugin doesn't expose volume control — Android uses the hardware
// buttons. No-op on purpose (see PlayerBar showVolume in App.tsx).
export async function setVolume(_volume: number) {}

// --- State pushed by the plugin ---------------------------------------------
//
// The plugin emits its state on every tick (25ms in foreground, 250ms in
// background). Listening to it instead of polling from JS gives two things
// polling doesn't: a real track end (`status: 'ended'`) instead of guessing
// it by comparing position against duration, and play/pause toggled from the
// notification or the lockscreen.
//
// For this to stay alive with the app in the background, `MainActivity.kt`
// prevents the WebView from pausing (see the reason there).

export type PlaybackEvent =
  | { type: 'position'; ms: number }
  | { type: 'playing'; value: boolean }
  | { type: 'ended' };

let endedSent = false;

/// How long before the end the track is switched while the app is off screen.
///
/// When the player reaches `STATE_ENDED`, the plugin's notification stops
/// being "ongoing" and its service LEAVES foreground. Starting the next track
/// brings it back, and `Service.startForeground()` from background is
/// forbidden on Android 12+: it crashes with
/// ForegroundServiceStartNotAllowedException. By switching tracks while the
/// current one is still playing, the notification never stops being
/// "ongoing", the service never leaves foreground, and the problem doesn't
/// exist — it's the same path that already works when tapping "next" on the
/// notification.
///
/// Only applies with the app off screen: in foreground the process has
/// plenty of permission and there's no need to trim the track at all. The
/// margin has to cover a couple of plugin ticks, which in background are 250ms.
const NEAR_END_LEAD_MS = 500;

let appVisible = true;
let endedEarly = false;

/// The app's real visibility, pushed by `MainActivity` (see the
/// `eval` in onPause/onResume). `document.visibilityState` isn't used because
/// MainActivity resumes the WebView by hand when the Activity pauses, so the
/// document keeps reporting itself as visible.
export function setAppVisible(visible: boolean) {
  appVisible = visible;
}

/// Discards the derived state after a deliberate position change (seek,
/// stop, new track).
function quiet() {
  endedSent = false;
  endedEarly = false;
}

/// Translates the plugin's stream of states into events that make sense for
/// the app. `emit` receives position at most every `POSITION_MS` (the native
/// tick is too fast to put into React state), but seeks and track end fire
/// instantly.
const POSITION_MS = 200;
let lastPositionEmit = 0;

let lastReportedPlaying: boolean | null = null;

function handleState(st: NativeAudioState, emit: (e: PlaybackEvent) => void) {
  const now = Date.now();
  const ms = Math.round((st.currentTime || 0) * 1000);

  // Play/pause can also be toggled from the notification and the lockscreen,
  // not just from the app: without this, the app's button keeps showing a
  // state that's no longer true.
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

  const durationMs = Math.round((st.duration || 0) * 1000);
  if (
    !appVisible &&
    !endedEarly &&
    st.isPlaying &&
    durationMs > 0 &&
    durationMs - ms <= NEAR_END_LEAD_MS
  ) {
    endedEarly = true;
    emit({ type: 'ended' });
    return;
  }

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
