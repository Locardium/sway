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
export const listTracks = () => invoke<Track[]>('list_tracks');

// Playback (rodio/symphonia en Rust).
export const playTrack = (id: number) => invoke<void>('play_track', { id });
export const pausePlayback = () => invoke<void>('pause_playback');
export const resumePlayback = () => invoke<void>('resume_playback');
export const stopPlayback = () => invoke<void>('stop_playback');
export const seekTo = (secs: number) => invoke<void>('seek_to', { secs });
export const playbackPosition = () => invoke<number>('playback_position');
