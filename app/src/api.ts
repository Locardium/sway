import { invoke } from '@tauri-apps/api/core';
import { isAndroid } from './platform';
import * as nativeAudio from './nativeAudio';

export interface Track {
  id: number;
  path: string;
  title: string;
  artist: string;
  album: string;
  genre: string;
  durationMs: number;
  bpm: number | null;
  /// The file is on this device. `false` = the row stayed but the audio was
  /// freed by selective sync, or hasn't been downloaded yet.
  present: boolean;
  /// Part of what this device syncs. `false` = none of its playlists are
  /// checked; the file that's already here is left alone, but it won't be
  /// downloaded or updated.
  ///
  /// Together with `present` it decides how it looks: out of scope with a
  /// file shows dimmed and doesn't play; out of scope without a file is
  /// hidden from the main view.
  inScope: boolean;
  uid: string | null;
}

export type NodeKind = 'folder' | 'playlist';

export interface PlaylistNode {
  id: number;
  /// Identity shared across devices (used by the scope editor).
  uid: string | null;
  name: string;
  kind: NodeKind;
  parentId: number | null;
  position: number;
  trackCount: number;
  /// How many of those tracks have a file here. This is what's shown when
  /// opening an unchecked playlist.
  presentCount: number;
  /// How many of those tracks still take up space **because of this
  /// playlist**: there's a file here and they're not in scope. A track that's
  /// also in a checked playlist still shows, but doesn't count here — its
  /// file is held by the other one, so this one will never release it.
  /// Decides whether the playlist stays in the tree, not what shows inside.
  strandedCount: number;
  /// Part of what this device syncs. `false` = unchecked: shows dimmed while
  /// still taking up space and disappears from the main view once freed. The
  /// scope editor always shows it.
  inScope: boolean;
}

// Playlists / folders (virtual hierarchy).
export const listPlaylists = () => invoke<PlaylistNode[]>('list_playlists');
export const createPlaylist = (name: string, kind: NodeKind, parentId: number | null) =>
  invoke<number>('create_playlist', { name, kind, parentId });
export const renamePlaylist = (id: number, name: string) =>
  invoke<void>('rename_playlist', { id, name });
export const deletePlaylist = (id: number) => invoke<void>('delete_playlist', { id });
export const movePlaylist = (id: number, parentId: number | null, index: number) =>
  invoke<void>('move_playlist', { id, parentId, index });

// Tracks within a playlist.
export const playlistTracks = (playlistId: number) =>
  invoke<Track[]>('playlist_tracks', { playlistId });
export const addTracksToPlaylist = (playlistId: number, trackIds: number[]) =>
  invoke<number>('add_tracks_to_playlist', { playlistId, trackIds });
export const removeTracksFromPlaylist = (playlistId: number, trackIds: number[]) =>
  invoke<void>('remove_tracks_from_playlist', { playlistId, trackIds });
export const reorderPlaylistTracks = (playlistId: number, trackIds: number[], index: number) =>
  invoke<void>('reorder_playlist_tracks', { playlistId, trackIds, index });

// Import: scans a folder, reads tags and inserts tracks. Returns how many.
export const importFolder = (folder: string) => invoke<number>('import_folder', { folder });
// Import of loose files/folders (OS drop). Returns track ids.
export const importFiles = (paths: string[]) => invoke<number[]>('import_files', { paths });
// Import from a picker (Android/iOS): uri can be content:// or file://.
export const importFromUri = (uri: string, name: string) =>
  invoke<number>('import_from_uri', { uri, name });
export const listTracks = () => invoke<Track[]>('list_tracks');
// Deletes from the library (doesn't touch files on disk).
export const deleteTracks = (ids: number[]) => invoke<void>('delete_tracks', { ids });
// Ids of the playlists that contain the track.
export const trackPlaylists = (id: number) => invoke<number[]>('track_playlists', { id });

// Playback: desktop drives a Rust Player (rodio/symphonia) via invoke;
// Android/iOS don't have that Player — the native plugin (Media3/ExoPlayer)
// is controlled directly from JS (see nativeAudio.ts). Same contract, two
// implementations, chosen once per platform.
const android = isAndroid();

const desktopPlayTrack = (id: number) => invoke<void>('play_track', { id });
const desktopPausePlayback = () => invoke<void>('pause_playback');
const desktopResumePlayback = () => invoke<void>('resume_playback');
const desktopStopPlayback = () => invoke<void>('stop_playback');
const desktopSeekTo = (secs: number) => invoke<void>('seek_to', { secs });
const desktopPlaybackPosition = () => invoke<number>('playback_position');
const desktopSetVolume = (volume: number) => invoke<void>('set_volume', { volume });

// Android pushes its state via events (position, track end, and the
// notification buttons — see nativeAudio.ts). Desktop has no equivalent: it
// stays null there and the app falls back to the usual position polling.
export type { PlaybackEvent } from './nativeAudio';
export const subscribePlayback = android ? nativeAudio.subscribePlayback : null;
export const setAppVisible = nativeAudio.setAppVisible;

export const playTrack = android ? nativeAudio.playTrack : desktopPlayTrack;
export const pausePlayback = android ? nativeAudio.pausePlayback : desktopPausePlayback;
export const resumePlayback = android ? nativeAudio.resumePlayback : desktopResumePlayback;
export const stopPlayback = android ? nativeAudio.stopPlayback : desktopStopPlayback;
export const seekTo = android ? nativeAudio.seekTo : desktopSeekTo;
export const playbackPosition = android ? nativeAudio.playbackPosition : desktopPlaybackPosition;
export const setVolume = android ? nativeAudio.setVolume : desktopSetVolume;

// Embedded cover art as a data-URL (null if the file has none).
export const coverThumb = (id: number) => invoke<string | null>('cover_thumb', { id });
// Opens the OS file explorer showing the file.
export const revealTrack = (id: number) => invoke<void>('reveal_track', { id });

// Auto-sync of the iTunes Music Library.xml (Phase 2). "Sync now" always
// writes; syncXmlAfterChange is fire-and-forget and respects the toggle.
export const exportLibraryXmlNow = () => invoke<void>('export_library_xml_now');
export const syncXmlAfterChange = () => invoke<void>('sync_xml_after_change');
export const getAutoSyncXml = () => invoke<boolean>('get_auto_sync_xml');
export const setAutoSyncXml = (enabled: boolean) => invoke<void>('set_auto_sync_xml', { enabled });

// P2P sync on the LAN (Phase 5).

/// Has to match `discovery::PROTO` on the Rust side. A peer announcing a
/// different version is still listed, but flagged: better not to offer
/// syncing than to fail halfway through.
export const SYNC_PROTO = '1';

export interface Peer {
  uid: string;
  name: string;
  platform: string;
  proto: string;
  addrs: string[];
  port: number;
  lastSeen: number;
  paired: boolean;
  /// Visible on the network right now. Paired devices are still listed when
  /// they're not: they stay your devices even if the phone has no wifi.
  online: boolean;
}

/// [uid, name] of this device. The uid can't change (tombstones and clocks
/// reference it); the name is just for recognizing it from the other side.
export const deviceIdentity = () => invoke<[string, string]>('device_identity');
export const setDeviceName = (name: string) => invoke<void>('set_device_name', { name });
export const listPeers = () => invoke<Peer[]>('list_peers');

/// Sends a fresh mDNS query. Responses arrive via `peers-changed`, not in the
/// return value: peers answer whenever they answer.
export const refreshPeers = () => invoke<void>('refresh_peers');

/// Pairs, or if already paired, asks for the other side's counts. Returns
/// right away: the result arrives via events, because on the other side
/// someone might take a while to confirm the code.
export const connectPeer = (uid: string) => invoke<void>('connect_peer', { uid });
export const confirmPairing = (uid: string, accept: boolean) =>
  invoke<boolean>('confirm_pairing', { uid, accept });
export const unpairDevice = (uid: string) => invoke<void>('unpair_device', { uid });

/// Pairs with a file server, which lives outside the local network and so
/// doesn't show up on its own. This one does answer in the same call — on the
/// other side there's no one taking their time to decide — and returns the
/// name the server declared.
export const pairWithServer = (host: string, port: number, token: string) =>
  invoke<string>('pair_with_server', { host, port, token });

/// Platform the server declares. The device list uses it to avoid showing it
/// as something that should appear on the network.
export const PLATFORM_SERVER = 'server';

// --- Metered network and battery (Phase 6.7) -------------------------------

/// Each field can be `null`, and that means "unknown" — which is different
/// from knowing it's false. A desktop PC returns `batteryPct: null` because
/// it has no battery, and with that the screen knows not to offer the
/// option.
export interface Conditions {
  metered: boolean | null;
  batteryPct: number | null;
  charging: boolean | null;
}

export interface SyncLimits {
  /// Sync with the server even on a metered connection.
  onMetered: boolean;
  /// Below this percentage, it won't sync on its own. 0 = no limit.
  minBattery: number;
}

export const syncConditions = () =>
  invoke<{ now: Conditions; limits: SyncLimits }>('sync_conditions');
export const setSyncLimits = (onMetered: boolean, minBattery: number) =>
  invoke<void>('set_sync_limits', { onMetered, minBattery });

/// What the screen can measure and Rust can't.
///
/// On Android, network and battery state can only be read from Java, and the
/// JNI context isn't initialized on the Rust side. The webview does have
/// `navigator.getBattery()` and `navigator.connection`, so they're reported
/// from here. Not needed on desktop: Rust reads them directly.
export const reportConditions = (c: Conditions) =>
  invoke<void>('report_conditions', {
    metered: c.metered,
    batteryPct: c.batteryPct,
    charging: c.charging,
  });

interface NetworkInformationLike {
  type?: string;
  effectiveType?: string;
  saveData?: boolean;
  addEventListener?: (type: string, cb: () => void) => void;
}

interface BatteryLike {
  level: number;
  charging: boolean;
  addEventListener?: (type: string, cb: () => void) => void;
}

/// Measures what the webview can measure. Anything unknowable comes back as
/// `null`, never made up: blocking a sync over a guessed value would be worse
/// than not blocking it.
async function measure(): Promise<Conditions> {
  const nav = navigator as Navigator & {
    connection?: NetworkInformationLike;
    getBattery?: () => Promise<BatteryLike>;
  };

  let metered: boolean | null = null;
  const conn = nav.connection;
  if (conn) {
    // `type` is the only thing that really answers the question. `saveData`
    // is the user asking to save data, which for these purposes is the same.
    if (conn.type === 'cellular') metered = true;
    else if (conn.type === 'wifi' || conn.type === 'ethernet') metered = false;
    if (conn.saveData) metered = true;
  }

  let batteryPct: number | null = null;
  let charging: boolean | null = null;
  if (nav.getBattery) {
    try {
      const b = await nav.getBattery();
      batteryPct = Math.round(b.level * 100);
      charging = b.charging;
    } catch {
      // No battery or no permission: stays null, which is the honest answer.
    }
  }
  return { metered, batteryPct, charging };
}

/// Starts periodic reporting. Only on Android: on desktop Rust reads better
/// than the webview can, and overwriting its data with `null` would lose
/// information.
export function watchConditions(): () => void {
  if (!android) return () => {};
  let stopped = false;
  const push = () => {
    if (stopped) return;
    measure()
      .then((c) => reportConditions(c))
      .catch(() => {});
  };
  push();
  // The network changes on its own (wifi dropping, data kicking in) and the
  // battery drains slowly: a minute covers both and doesn't wake the phone up.
  const timer = setInterval(push, 60_000);
  return () => {
    stopped = true;
    clearInterval(timer);
  };
}

/// Payload of the `pairing-request` event: `code` must be shown while waiting
/// for the user to confirm. The same code appears on both screens.
export interface PairingRequest {
  uid: string;
  name: string;
  platform: string;
  code: string;
  incoming: boolean;
}

export interface PairingDone {
  uid: string;
  name: string;
  ok: boolean;
  error: string | null;
}

/// What would happen if syncing now (Phase 5.3). It's only calculated and
/// shown: none of this executes yet.
export interface SyncPlan {
  pullFiles: { trackUid: string; hash: string; filename: string; size: number }[];
  pushFiles: { trackUid: string; hash: string; filename: string; size: number }[];
  pullMeta: number;
  pushMeta: number;
  pullPlaylists: number;
  pushPlaylists: number;
  pullMemberships: number;
  pushMemberships: number;
  deletesIn: number;
  deletesOut: number;
  /// Local tracks that still don't have a hash: they don't take part in the plan.
  unhashed: number;
  /// Files not pulled / not pushed because they fell outside the selective
  /// scope. Not pending work: this is selective sync working as intended.
  outOfScopeIn: number;
  outOfScopeOut: number;
}

export interface SyncPlanEvent {
  uid: string;
  name: string;
  plan: SyncPlan;
  bytesIn: number;
  bytesOut: number;
}

export const previewSync = (uid: string) => invoke<void>('preview_sync', { uid });

/// Executes the plan's file transfer (Phase 5.4). Files only: metadata,
/// playlists, and deletions arrive in 5.5/5.6.
export const syncFiles = (uid: string) => invoke<void>('sync_files', { uid });

export interface SyncProgress {
  uid: string;
  fileIndex: number;
  fileTotal: number;
  filename: string;
  done: number;
  total: number;
  sending: boolean;
}

export interface SyncDone {
  uid: string;
  name: string;
  received: number;
  sent: number;
  failed: number;
  bytes: number;
  /// Organization records applied (metadata, playlists, memberships).
  organized: number;
  /// Triggered by auto-sync, not the user.
  auto: boolean;
  error: string | null;
}

// --- Selective scope and space (Phase 5.7) ----------------------------------

export interface Scope {
  /// all | selected
  mode: string;
  /// What THAT device does: `both | send | receive | off`. Between two
  /// devices, something only moves if one sends and the other receives.
  direction: string;
  /// Uids checked by hand. What hangs off a checked folder is included
  /// without showing up here — the backend resolves the tree.
  selected: string[];
}

export const getScope = (deviceUid: string) => invoke<Scope>('get_scope', { deviceUid });
export const setScopeMode = (deviceUid: string, mode: string) =>
  invoke<void>('set_scope_mode', { deviceUid, mode });
export const setScopeDirection = (deviceUid: string, direction: string) =>
  invoke<void>('set_scope_direction', { deviceUid, direction });
export const setScopePlaylist = (deviceUid: string, playlistUid: string, selected: boolean) =>
  invoke<void>('set_scope_playlist', { deviceUid, playlistUid, selected });
/// Several rows in one go: checking a folder changes every playlist inside
/// it, and doing it one by one would be N round trips and N commits.
export const setScopePlaylists = (deviceUid: string, changes: { uid: string; on: boolean }[]) =>
  invoke<void>('set_scope_playlists', { deviceUid, changes });

export interface Storage {
  libraryBytes: number;
  tracksPresent: number;
  tracksAbsent: number;
  /// What can be freed right now without risking anything: out of scope and
  /// with a confirmed copy on another paired device.
  freeableCount: number;
  freeableBytes: number;
}

export const storageStatus = () => invoke<Storage>('storage_status');
/// Sends files out of scope to the trash (30 days). Returns [how many,
/// bytes]. Doesn't delete anything from the library: the rows stay.
export const freeSpace = () => invoke<[number, number]>('free_space');

export interface LogEntry {
  ts: number;
  kind: string;
  detail: string;
}

export const syncHistory = (uid: string, limit = 20) =>
  invoke<LogEntry[]>('sync_history', { uid, limit });

/// Auto-sync: when something changes here, when a device shows up, and every
/// so often as a safety net.
export const getAutoSyncP2p = () => invoke<boolean>('get_auto_sync_p2p');
export const setAutoSyncP2p = (enabled: boolean) =>
  invoke<void>('set_auto_sync_p2p', { enabled });

export interface PeerHello {
  uid: string;
  name: string;
  tracks: number;
  playlists: number;
  clockSkewMs: number;
}
