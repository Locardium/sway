# `tauri-plugin-native-audio` — Sway fork

Vendored from crates.io `tauri-plugin-native-audio` 1.0.5
(<https://github.com/uvarov-frontend/tauri-plugin-native-audio>, MIT OR
Apache-2.0). The licence files are kept next to this one.

## Why

Upstream's whole API is `initialize / setSource / play / pause / seekTo /
setRate / getState / getProgressCheckpoint / clearProgressCheckpoint / dispose /
addStateListener`. That is one media item and no volume, which blocks four
things Sway wants on Android:

- **volume and per-track gain** — the app's volume slider was hidden on Android
  (`showVolume={!isAndroid()}`) because there was nothing to call.
- **gapless playback** — the app can only load the next track after the current
  one reports `ended`, which is by definition a gap.
- **crossfade** — needs two players overlapping, and there was only one.
- **output-device selection** — `ExoPlayer.setPreferredAudioDevice` was never
  reachable.

None of it can be added from the outside: the runtime is a private `object` and
the commands are fixed at build time. Hence the fork.

## What changed

Everything below is additive — the upstream commands behave exactly as before,
and an app that never calls the new ones gets the old single-item behaviour.

### New commands

| Command | Kotlin method | What it does |
| --- | --- | --- |
| `set_volume` | `setVolume` | Master volume, `0..1`. |
| `set_source_gain` | `setSourceGain` | Replaces the gain of the item already playing (ReplayGain arriving late). |
| `set_next_source` | `setNextSource` | Stages what plays after the current item, or clears it with `src: null`. |
| `skip_to_next` | `skipToNext` | Jumps to the staged item now, honouring crossfade. |
| `set_crossfade` | `setCrossfade` | Overlap in seconds; `0` = off, which is what turns gapless on. |
| `list_output_devices` | `listOutputDevices` | The `AudioManager` output devices, with `id`, `type` and `name`. |
| `set_output_device` | `setOutputDevice` | `ExoPlayer.setPreferredAudioDevice`; `id: null` = system default. |

`set_source` and `set_next_source` also take an optional `gainDb`, applied on
top of the master volume for that item only. That is the primitive
ReplayGain/R128 normalisation needs: the app reads the tag and passes the
number, the plugin does the arithmetic per item.

### Two decks

`NativeAudioRuntime` now owns up to two `ExoPlayer`s.

- **Crossfade off** — one deck. The staged next item is appended to the same
  player's playlist, which is where ExoPlayer's gapless transition comes from.
- **Crossfade on** — the staged item is prepared on the idle deck and the two
  volumes are ramped past each other. The decks swap roles at the *start* of the
  ramp, so `getState` reports the incoming track while the outgoing one fades
  out behind it.

The `MediaSession` follows the active deck, and the notification's
`PlayerNotificationManager` is re-pointed with it (see `deckListeners`).

### Transport interception

Upstream's session player routes `seekToNext`/`seekToPrevious` to
`seekForward`/`seekBack` — 10-second jumps, never a track change — with no way
to reconfigure it. Sway used to work around that from `MainActivity` by
replacing `mediaSession.player` with its own `ForwardingPlayer` after polling
for the session to exist. That breaks the moment the runtime swaps decks,
because the replacement is silently dropped.

So the hook moved in here: `NativeAudioRuntime.transportInterceptor`. When set,
next/prev from the notification and the lock screen go to it instead, and it is
re-applied automatically on every deck swap.

### State payload

Added: `volume`, `trackId`, `nextTrackId`, `crossfade`, `outputDeviceId`,
`gainDb`. `trackId` is what tells the frontend that a gapless or crossfaded
transition happened — there is no `ended` event between items any more.

## iOS

Untouched, and it does **not** implement the new commands: they reject there.
Sway is Android-only on mobile (`isAndroid()` in `app/src/platform.ts`), and the
Swift sources are kept verbatim only so the crate still builds for that target.
