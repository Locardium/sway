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
  /// El archivo está en este dispositivo. `false` = la fila quedó pero el
  /// audio se liberó por sync selectiva, o todavía no se bajó.
  present: boolean;
  /// Entra en lo que este dispositivo sincroniza. `false` = ninguna de sus
  /// playlists está marcada; el archivo que ya está no se toca, pero no se va
  /// a bajar ni a actualizar.
  ///
  /// Con `present` decide cómo se ve: fuera de scope y con archivo se muestra
  /// apagado y no suena; fuera de scope y sin archivo se esconde de la vista
  /// principal.
  inScope: boolean;
  uid: string | null;
}

export type NodeKind = 'folder' | 'playlist';

export interface PlaylistNode {
  id: number;
  /// Identidad compartida entre dispositivos (la usa el editor de scope).
  uid: string | null;
  name: string;
  kind: NodeKind;
  parentId: number | null;
  position: number;
  trackCount: number;
  /// De cuántos de esos tracks hay archivo acá. Es lo que se ve al abrir una
  /// playlist desmarcada.
  presentCount: number;
  /// Cuántos de esos tracks siguen ocupando lugar **por esta playlist**: hay
  /// archivo acá y no entran en el scope. Un tema que además está en una
  /// playlist marcada se ve igual, pero no cuenta acá — su archivo lo sostiene
  /// la otra, así que ésta no lo va a soltar nunca. Decide si la playlist
  /// sigue en el árbol, no qué se muestra adentro.
  strandedCount: number;
  /// Entra en lo que este dispositivo sincroniza. `false` = desmarcada: se ve
  /// apagada mientras siga ocupando lugar y desaparece de la vista principal
  /// cuando se liberó. En el editor de scope se ve siempre.
  inScope: boolean;
}

// Playlists / folders (jerarquia virtual).
export const listPlaylists = () => invoke<PlaylistNode[]>('list_playlists');
export const createPlaylist = (name: string, kind: NodeKind, parentId: number | null) =>
  invoke<number>('create_playlist', { name, kind, parentId });
export const renamePlaylist = (id: number, name: string) =>
  invoke<void>('rename_playlist', { id, name });
export const deletePlaylist = (id: number) => invoke<void>('delete_playlist', { id });
export const movePlaylist = (id: number, parentId: number | null, index: number) =>
  invoke<void>('move_playlist', { id, parentId, index });

// Tracks dentro de una playlist.
export const playlistTracks = (playlistId: number) =>
  invoke<Track[]>('playlist_tracks', { playlistId });
export const addTracksToPlaylist = (playlistId: number, trackIds: number[]) =>
  invoke<number>('add_tracks_to_playlist', { playlistId, trackIds });
export const removeTracksFromPlaylist = (playlistId: number, trackIds: number[]) =>
  invoke<void>('remove_tracks_from_playlist', { playlistId, trackIds });
export const reorderPlaylistTracks = (playlistId: number, trackIds: number[], index: number) =>
  invoke<void>('reorder_playlist_tracks', { playlistId, trackIds, index });

// Import: escanea una carpeta, lee tags e inserta tracks. Devuelve cuantos.
export const importFolder = (folder: string) => invoke<number>('import_folder', { folder });
// Import de archivos/carpetas sueltos (drop del OS). Devuelve ids de tracks.
export const importFiles = (paths: string[]) => invoke<number[]>('import_files', { paths });
// Import desde un picker (Android/iOS): uri puede ser content:// o file://.
export const importFromUri = (uri: string, name: string) =>
  invoke<number>('import_from_uri', { uri, name });
export const listTracks = () => invoke<Track[]>('list_tracks');
// Borra de la biblioteca (no toca archivos en disco).
export const deleteTracks = (ids: number[]) => invoke<void>('delete_tracks', { ids });
// Ids de las playlists que contienen el track.
export const trackPlaylists = (id: number) => invoke<number[]>('track_playlists', { id });

// Playback: desktop mueve un Player de Rust (rodio/symphonia) via invoke;
// Android/iOS no tienen ese Player — el plugin nativo (Media3/ExoPlayer) se
// controla directo desde JS (ver nativeAudio.ts). Mismo contrato, dos
// implementaciones, elegidas una sola vez por plataforma.
const android = isAndroid();

const desktopPlayTrack = (id: number) => invoke<void>('play_track', { id });
const desktopPausePlayback = () => invoke<void>('pause_playback');
const desktopResumePlayback = () => invoke<void>('resume_playback');
const desktopStopPlayback = () => invoke<void>('stop_playback');
const desktopSeekTo = (secs: number) => invoke<void>('seek_to', { secs });
const desktopPlaybackPosition = () => invoke<number>('playback_position');
const desktopSetVolume = (volume: number) => invoke<void>('set_volume', { volume });

// Android empuja su estado por evento (posicion, fin de track y los botones
// de la notificacion — ver nativeAudio.ts). Desktop no tiene equivalente: ahi
// queda null y la app cae al poll de posicion de siempre.
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

// Caratula embebida como data-URL (null si el archivo no tiene).
export const coverThumb = (id: number) => invoke<string | null>('cover_thumb', { id });
// Abre el explorador del OS mostrando el archivo.
export const revealTrack = (id: number) => invoke<void>('reveal_track', { id });

// Auto-sync del iTunes Music Library.xml (Fase 2). "Sync now" siempre
// escribe; syncXmlAfterChange es fire-and-forget y respeta el toggle.
export const exportLibraryXmlNow = () => invoke<void>('export_library_xml_now');
export const syncXmlAfterChange = () => invoke<void>('sync_xml_after_change');
export const getAutoSyncXml = () => invoke<boolean>('get_auto_sync_xml');
export const setAutoSyncXml = (enabled: boolean) => invoke<void>('set_auto_sync_xml', { enabled });

// Sync P2P en la LAN (Fase 5).

/// Tiene que coincidir con `discovery::PROTO` del lado Rust. Un peer que
/// anuncia otra version se lista igual, pero marcado: mejor no ofrecer
/// sincronizar que fallar a mitad de camino.
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
  /// Visible en la red ahora mismo. Los pareados se listan igual cuando no
  /// están: siguen siendo tus dispositivos aunque el celu esté sin wifi.
  online: boolean;
}

/// [uid, name] de este dispositivo. El uid no se puede cambiar (lo
/// referencian tombstones y clocks); el nombre es solo para reconocerlo desde
/// el otro lado.
export const deviceIdentity = () => invoke<[string, string]>('device_identity');
export const setDeviceName = (name: string) => invoke<void>('set_device_name', { name });
export const listPeers = () => invoke<Peer[]>('list_peers');

/// Manda una consulta mDNS nueva. Las respuestas llegan por `peers-changed`,
/// no en el retorno: los peers contestan cuando contestan.
export const refreshPeers = () => invoke<void>('refresh_peers');

/// Vincula, o si ya está vinculado pide los conteos del otro lado. Vuelve
/// enseguida: el resultado llega por eventos, porque del otro lado puede
/// haber una persona tardando en confirmar el código.
export const connectPeer = (uid: string) => invoke<void>('connect_peer', { uid });
export const confirmPairing = (uid: string, accept: boolean) =>
  invoke<boolean>('confirm_pairing', { uid, accept });
export const unpairDevice = (uid: string) => invoke<void>('unpair_device', { uid });

/// Vincula con un server de archivo, que vive fuera de la red local y por eso
/// no aparece solo. Esta sí contesta en la misma llamada —del otro lado no hay
/// nadie que tarde en decidir— y devuelve el nombre que declaró el server.
export const pairWithServer = (host: string, port: number, token: string) =>
  invoke<string>('pair_with_server', { host, port, token });

/// Plataforma que declara el server. La lista de dispositivos la usa para no
/// mostrarlo como algo que debería aparecer en la red.
export const PLATFORM_SERVER = 'server';

/// Payload del evento `pairing-request`: hay que mostrar `code` y esperar que
/// el usuario confirme. El mismo código aparece en las dos pantallas.
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

/// Lo que pasaría si se sincronizara ahora (Fase 5.3). Sólo se calcula y se
/// muestra: nada de esto se ejecuta todavía.
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
  /// Tracks locales que todavía no tienen hash: no participan del plan.
  unhashed: number;
  /// Archivos que no se traen / no se mandan porque quedaron fuera del scope
  /// selectivo. No son trabajo pendiente: es la sync selectiva funcionando.
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

/// Ejecuta la transferencia de archivos del plan (Fase 5.4). Sólo archivos:
/// metadata, playlists y borrados llegan en 5.5/5.6.
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
  /// Registros de organización aplicados (metadata, playlists, membresías).
  organized: number;
  /// Lo disparó el sync automático, no el usuario.
  auto: boolean;
  error: string | null;
}

// --- Scope selectivo y espacio (Fase 5.7) -----------------------------------

export interface Scope {
  /// all | selected
  mode: string;
  /// Qué hace ESE dispositivo: `both | send | receive | off`. Entre dos, algo
  /// se mueve sólo si uno manda y el otro recibe.
  direction: string;
  /// Uids marcados a mano. Lo que cuelga de una carpeta marcada entra sin
  /// figurar acá — el árbol lo resuelve el backend.
  selected: string[];
}

export const getScope = (deviceUid: string) => invoke<Scope>('get_scope', { deviceUid });
export const setScopeMode = (deviceUid: string, mode: string) =>
  invoke<void>('set_scope_mode', { deviceUid, mode });
export const setScopeDirection = (deviceUid: string, direction: string) =>
  invoke<void>('set_scope_direction', { deviceUid, direction });
export const setScopePlaylist = (deviceUid: string, playlistUid: string, selected: boolean) =>
  invoke<void>('set_scope_playlist', { deviceUid, playlistUid, selected });
/// Varias filas de una: marcar una carpeta cambia todas las playlists de
/// adentro, y de a una son N viajes y N commits.
export const setScopePlaylists = (deviceUid: string, changes: { uid: string; on: boolean }[]) =>
  invoke<void>('set_scope_playlists', { deviceUid, changes });

export interface Storage {
  libraryBytes: number;
  tracksPresent: number;
  tracksAbsent: number;
  /// Lo que se puede liberar ahora sin arriesgar nada: fuera de scope y con
  /// copia confirmada en otro dispositivo vinculado.
  freeableCount: number;
  freeableBytes: number;
}

export const storageStatus = () => invoke<Storage>('storage_status');
/// Manda a la papelera (30 días) los archivos fuera de scope. Devuelve
/// [cuántos, bytes]. No borra nada de la biblioteca: las filas quedan.
export const freeSpace = () => invoke<[number, number]>('free_space');

export interface LogEntry {
  ts: number;
  kind: string;
  detail: string;
}

export const syncHistory = (uid: string, limit = 20) =>
  invoke<LogEntry[]>('sync_history', { uid, limit });

/// Sync automático: al cambiar algo acá, cuando aparece un dispositivo, y
/// cada tanto como red de contención.
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
