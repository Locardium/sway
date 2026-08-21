// Playback backend for Android/iOS: the vendored native-audio plugin
// (Media3/ExoPlayer, `crates/native-audio`), validated in Phase 0.5
// (spike-android-audio/). Unlike desktop, this plugin is controlled directly
// from JS — there's no Rust Player to wrap here, the native plugin itself is
// the bridge.
import { invoke } from '@tauri-apps/api/core';
import {
  initialize,
  setSource,
  setNextSource,
  skipToNext,
  play,
  pause,
  seekTo as nativeSeekTo,
  getState,
  addStateListener,
  setVolume as nativeSetVolume,
  setCrossfade as nativeSetCrossfade,
  listOutputDevices as nativeListOutputDevices,
  setOutputDevice as nativeSetOutputDevice,
  type NativeAudioOutputDevice,
  type NativeAudioState,
} from './nativeAudioPlugin';

export type { NativeAudioOutputDevice };

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

/// What the plugin has staged behind the current track, and how long the
/// overlap is. Both are mirrored here so `playTrack` can tell an ordinary
/// "next" apart from one the plugin can already crossfade into.
let stagedNextId: number | null = null;
let crossfadeSecs = 0;

/// The track the app believes is playing. Compared against the plugin's
/// `trackId` to spot a transition the app didn't ask for — which is exactly
/// what gapless and crossfade produce.
let lastTrackId: number | null = null;

/// A track change the app asked for and the plugin hasn't reported yet.
///
/// The plugin keeps pushing state while `setSource` is in flight, and that
/// state still names the OLD track. Without this, those few ticks look like a
/// transition back to the track we just left — the app would follow them and
/// bounce. States are held back until the id we asked for shows up, or until
/// the deadline says the request went nowhere.
let awaitedId: number | null = null;
let awaitedUntil = 0;
/// How long to keep holding them back. Long enough to cover opening a file off
/// slow internal storage, short enough that a `setSource` that silently failed
/// doesn't freeze the UI.
const AWAIT_TRACK_MS = 3000;

function expect(id: number) {
  awaitedId = id;
  awaitedUntil = Date.now() + AWAIT_TRACK_MS;
}

/// How many `setNextSource` calls haven't come back yet. While any is in
/// flight the plugin's `nextTrackId` is still the previous one, so it isn't
/// trusted over what was just requested.
let stagingInFlight = 0;

export async function playTrack(id: number) {
  await ensureInitialized();
  // Already staged and there's an overlap configured: this is the transition
  // the plugin was set up for, so let it fade instead of cutting.
  if (id === stagedNextId && crossfadeSecs > 0) {
    expect(id);
    const st = await skipToNext();
    if (st.skipped) {
      quiet();
      return;
    }
    // Nothing was staged after all; fall through and load it the plain way.
  }
  const path = await getTrackPath(id);
  quiet();
  expect(id);
  stagedNextId = null;
  await setSource({ src: toFileUri(path), id });
  await play();
}

/// What plays after the current track, or `null` to clear it.
///
/// With crossfade off the plugin appends it to the same ExoPlayer's playlist,
/// which is where the gapless transition comes from; with crossfade on it
/// prepares a second player and starts it early. Either way the track change
/// arrives as a `advanced` event instead of `ended` + a new `playTrack`.
export async function setNextTrack(id: number | null) {
  await ensureInitialized();
  if (id == null) {
    if (stagedNextId == null) return;
    stagedNextId = null;
    stagingInFlight++;
    try {
      await setNextSource(null);
    } finally {
      stagingInFlight--;
    }
    return;
  }
  if (id === stagedNextId) return;
  const path = await getTrackPath(id);
  stagedNextId = id;
  stagingInFlight++;
  try {
    await setNextSource({ src: toFileUri(path), id });
  } finally {
    stagingInFlight--;
  }
}

export async function pausePlayback() {
  await pause();
}

export async function resumePlayback() {
  await play();
}

export async function stopPlayback() {
  quiet();
  lastTrackId = null;
  awaitedId = null;
  stagedNextId = null;
  stagingInFlight++;
  try {
    await setNextSource(null);
  } finally {
    stagingInFlight--;
  }
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

/// Master volume, 0..1. Applied by `ExoPlayer.setVolume` on whichever deck is
/// active, on top of the track's own gain.
export async function setVolume(volume: number) {
  await ensureInitialized();
  await nativeSetVolume(volume);
}

/// Overlap between one track and the next, in seconds. `0` turns it off, and
/// that is also what makes the transition gapless: with no overlap the staged
/// track goes into the same player's playlist.
export async function setCrossfade(seconds: number) {
  await ensureInitialized();
  crossfadeSecs = seconds;
  await nativeSetCrossfade(seconds);
}

export async function listOutputDevices(): Promise<NativeAudioOutputDevice[]> {
  await ensureInitialized();
  return nativeListOutputDevices();
}

/// `null` hands the choice back to Android, which is also what happens if the
/// pinned device is unplugged — better than going silent.
export async function setOutputDevice(id: number | null) {
  await ensureInitialized();
  await nativeSetOutputDevice(id);
}

// --- State pushed by the plugin ---------------------------------------------
//
// The plugin emits its state on every tick (25ms in foreground, 250ms in
// background). Listening to it instead of polling from JS gives three things
// polling doesn't: a real track end (`status: 'ended'`) instead of guessing
// it by comparing position against duration, play/pause toggled from the
// notification or the lockscreen, and — since the fork — the id of the track
// that is playing, which is the only sign that a queued one took over.
//
// For this to stay alive with the app in the background, `MainActivity.kt`
// prevents the WebView from pausing (see the reason there).

export type PlaybackEvent =
  | { type: 'position'; ms: number }
  | { type: 'playing'; value: boolean }
  /// A staged track took over on its own (gapless or crossfaded). There is no
  /// `ended` before it: the player never stopped.
  | { type: 'advanced'; id: number }
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
///
/// It also only applies when nothing is staged. With a queued track the player
/// moves on by itself and never reaches `STATE_ENDED` in between, so there is
/// no moment when the service could drop out of foreground — that whole class
/// of crash stops existing.
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
/// tick is too fast to put into React state), but seeks, transitions and track
/// end fire instantly.
const POSITION_MS = 200;
let lastPositionEmit = 0;

let lastReportedPlaying: boolean | null = null;

function handleState(st: NativeAudioState, emit: (e: PlaybackEvent) => void) {
  const now = Date.now();
  const ms = Math.round((st.currentTime || 0) * 1000);
  const id = st.trackId ?? null;

  if (awaitedId != null) {
    if (id === awaitedId) {
      awaitedId = null;
      lastTrackId = id;
    } else if (now < awaitedUntil) {
      // Still describing the track we're leaving. Reporting any of it — the
      // old position most of all — would make the bar jump backwards.
      return;
    } else {
      // The change never arrived. Take whatever is playing as the truth
      // rather than emitting a transition nobody asked for.
      awaitedId = null;
      lastTrackId = id;
    }
  } else if (id != null && id !== lastTrackId) {
    // A queued track took over. Nothing ended and nothing was asked for, so
    // this is the only place the change shows up.
    const first = lastTrackId == null;
    lastTrackId = id;
    quiet();
    if (!first) {
      emit({ type: 'advanced', id });
      lastPositionEmit = 0;
    }
  }

  // What the plugin says is staged wins over what was requested — a track that
  // failed to prepare is dropped there, and the app has to hear about it to
  // queue another one. Except while a staging call is still in flight, when
  // this is by definition the answer to the previous question.
  if (stagingInFlight === 0) stagedNextId = st.nextTrackId ?? null;
  if (typeof st.crossfade === 'number') crossfadeSecs = st.crossfade;

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
    stagedNextId == null &&
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
