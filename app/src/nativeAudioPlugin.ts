// JS side of `crates/native-audio`, Sway's fork of `tauri-plugin-native-audio`
// (see its FORK.md). It replaces the `tauri-plugin-native-audio-api` package:
// the fork adds commands that package doesn't know about, and publishing a
// package just to import six more `invoke` calls isn't worth it.
//
// Command names go over the wire in snake_case and land on lowerCamelCase
// Kotlin methods — tauri runs them through `heck::AsLowerCamelCase` before
// handing them to the Android plugin. `permissions/` on the Rust side uses the
// snake_case spelling, so both have to match what's below.
import { addPluginListener, invoke } from '@tauri-apps/api/core';

const PLUGIN_NAME = 'native-audio';
const STATE_EVENT = 'native_audio_state';

const call = <T>(command: string, payload?: Record<string, unknown>) =>
  invoke<T>(`plugin:${PLUGIN_NAME}|${command}`, payload);

export type NativeAudioStatus = 'idle' | 'loading' | 'playing' | 'ended' | 'error';

export interface NativeAudioState {
  status: NativeAudioStatus;
  currentTime: number;
  duration: number;
  isPlaying: boolean;
  buffering: boolean;
  rate: number;
  /// Master volume, 0..1. Independent of `gainDb`, which is per track.
  volume: number;
  /// Gain of the track playing right now, in dB (ReplayGain/R128).
  gainDb: number;
  /// Overlap between tracks, in seconds. 0 = off, which is what makes the
  /// transition gapless instead.
  crossfade: number;
  /// Id of the track playing right now. When a queued track starts there is no
  /// `ended` in between — this changing is the only signal.
  trackId?: number;
  /// Id of the track staged behind it, if any.
  nextTrackId?: number;
  outputDeviceId?: number;
  error?: string;
}

export interface NativeAudioSource {
  src: string;
  id?: number;
  title?: string;
  artist?: string;
  artworkUrl?: string;
  /// Per-track level in dB, cut or boost. The cut half rides `ExoPlayer.volume`
  /// and the boost half a `LoudnessEnhancer`, because that volume saturates at
  /// 1.0 — see `applyVolumeLocked` in the plugin.
  gainDb?: number;
  /// Playable region in ms, from the analyzer. The silence a file carries at
  /// its edges is what the gap between two tracks is made of, so playing only
  /// between these two is what makes the handover gapless. `0` for either
  /// means the file's own edge.
  leadMs?: number;
  audioEndMs?: number;
}

export interface NativeAudioOutputDevice {
  id: number;
  /// Readable kind ("Bluetooth", "Wired headphones", …), from AudioDeviceInfo.
  type: string;
  name: string;
}

export interface NativeAudioProgressCheckpoint {
  id: number;
  currentTime: number;
  updatedAtMs: number;
  status?: NativeAudioStatus;
}

export const initialize = () => call<NativeAudioState>('initialize');
export const setSource = (payload: NativeAudioSource) =>
  call<NativeAudioState>('set_source', payload as unknown as Record<string, unknown>);
export const play = () => call<NativeAudioState>('play');
export const pause = () => call<NativeAudioState>('pause');
export const seekTo = (position: number) => call<NativeAudioState>('seek_to', { position });
export const setRate = (rate: number) => call<NativeAudioState>('set_rate', { rate });
export const getState = () => call<NativeAudioState>('get_state');
export const getProgressCheckpoint = () =>
  call<NativeAudioProgressCheckpoint | null>('get_progress_checkpoint');
export const clearProgressCheckpoint = () => call<void>('clear_progress_checkpoint');
export const dispose = () => call<void>('dispose');

// --- Added by the fork ------------------------------------------------------

export const setVolume = (volume: number) => call<NativeAudioState>('set_volume', { volume });

/// Replaces the gain of the track already playing, without restarting it.
export const setSourceGain = (gainDb: number) =>
  call<NativeAudioState>('set_source_gain', { gainDb });

/// What plays after the current track, or `null` to clear it. With crossfade
/// off this goes into the same ExoPlayer's playlist (gapless); with crossfade
/// on it's prepared on a second player and started early.
export const setNextSource = (payload: NativeAudioSource | null) =>
  call<NativeAudioState>('set_next_source', (payload ?? {}) as unknown as Record<string, unknown>);

/// Starts the staged track now. `skipped` is false when nothing was staged —
/// the caller then has to fall back to `setSource`.
export const skipToNext = () =>
  call<NativeAudioState & { skipped: boolean }>('skip_to_next');

export const setCrossfade = (seconds: number) =>
  call<NativeAudioState>('set_crossfade', { seconds });

export const listOutputDevices = async (): Promise<NativeAudioOutputDevice[]> => {
  const res = await call<{ devices: NativeAudioOutputDevice[] }>('list_output_devices');
  return res.devices ?? [];
};

/// `null` gives the choice back to Android. An id that no longer exists does
/// the same rather than going silent.
export const setOutputDevice = (id: number | null) =>
  call<NativeAudioState>('set_output_device', { id });

/// Returns the unsubscribe. `addPluginListener` resolves to a `PluginListener`
/// object, not to a function — the published package typed it as one, which
/// meant unsubscribing threw. Wrapped here so callers get what the type says.
export const addStateListener = async (
  handler: (state: NativeAudioState) => void,
): Promise<() => void> => {
  const listener = await addPluginListener(PLUGIN_NAME, STATE_EVENT, handler);
  return () => {
    void listener.unregister();
  };
};
