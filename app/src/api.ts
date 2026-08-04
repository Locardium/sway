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
}

export type NodeKind = 'folder' | 'playlist';

export interface PlaylistNode {
  id: number;
  name: string;
  kind: NodeKind;
  parentId: number | null;
  position: number;
  trackCount: number;
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
