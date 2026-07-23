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

// Playback (rodio/symphonia en Rust).
export const playTrack = (id: number) => invoke<void>('play_track', { id });
export const pausePlayback = () => invoke<void>('pause_playback');
export const resumePlayback = () => invoke<void>('resume_playback');
export const stopPlayback = () => invoke<void>('stop_playback');
export const seekTo = (secs: number) => invoke<void>('seek_to', { secs });
export const playbackPosition = () => invoke<number>('playback_position');
export const setVolume = (volume: number) => invoke<void>('set_volume', { volume });

// Caratula embebida como data-URL (null si el archivo no tiene).
export const coverThumb = (id: number) => invoke<string | null>('cover_thumb', { id });
// Abre el explorador del OS mostrando el archivo.
export const revealTrack = (id: number) => invoke<void>('reveal_track', { id });
