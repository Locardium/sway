import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { open } from '@tauri-apps/plugin-dialog';
import {
  Track,
  PlaylistNode,
  NodeKind,
  importFolder,
  listTracks,
  listPlaylists,
  createPlaylist,
  renamePlaylist,
  deletePlaylist,
  movePlaylist,
  playlistTracks,
  addTracksToPlaylist,
  removeTracksFromPlaylist,
  reorderPlaylistTracks,
  playTrack,
  pausePlayback,
  resumePlayback,
  stopPlayback,
  playbackPosition,
  seekTo,
} from './api';
import Sidebar, { Selection } from './components/Sidebar';
import TrackTable from './components/TrackTable';
import PlayerBar from './components/PlayerBar';

export default function App() {
  const [library, setLibrary] = useState<Track[]>([]);
  const [nodes, setNodes] = useState<PlaylistNode[]>([]);
  const [selection, setSelection] = useState<Selection>({ type: 'library' });
  const [plTracks, setPlTracks] = useState<Track[]>([]);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [search, setSearch] = useState('');

  const [currentId, setCurrentId] = useState<number | null>(null);
  const [paused, setPaused] = useState(false);
  const [posMs, setPosMs] = useState(0);
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState('');
  // Cola de reproduccion: orden visible al momento de dar play.
  const queueRef = useRef<number[]>([]);

  const refreshLibrary = useCallback(async () => {
    setLibrary(await listTracks());
  }, []);
  const refreshPlaylists = useCallback(async () => {
    setNodes(await listPlaylists());
  }, []);
  const refreshPlaylistTracks = useCallback(async () => {
    if (selection.type === 'playlist') setPlTracks(await playlistTracks(selection.id));
  }, [selection]);

  // Carga inicial con reintentos (el backend puede tardar en registrar estado).
  useEffect(() => {
    let tries = 0;
    const attempt = async () => {
      try {
        await Promise.all([refreshLibrary(), refreshPlaylists()]);
      } catch {
        if (tries++ < 5) setTimeout(attempt, 300);
      }
    };
    attempt();
  }, [refreshLibrary, refreshPlaylists]);

  // Al cambiar seleccion, carga tracks de la playlist y limpia seleccion de filas.
  useEffect(() => {
    setSelected(new Set());
    setSearch('');
    if (selection.type === 'playlist') {
      playlistTracks(selection.id).then(setPlTracks).catch(() => setPlTracks([]));
    }
  }, [selection]);

  // Poll de posicion.
  useEffect(() => {
    const t = setInterval(async () => {
      if (currentId != null && !paused) {
        try {
          setPosMs((await playbackPosition()) * 1000);
        } catch {}
      }
    }, 500);
    return () => clearInterval(t);
  }, [currentId, paused]);

  const baseTracks = selection.type === 'library' ? library : plTracks;
  const visibleTracks = useMemo(() => {
    const q = search.trim().toLowerCase();
    if (!q) return baseTracks;
    return baseTracks.filter((t) =>
      [t.title, t.artist, t.album, t.genre].some((f) => f.toLowerCase().includes(q)),
    );
  }, [baseTracks, search]);

  const current = useMemo(
    () => library.find((t) => t.id === currentId) ?? plTracks.find((t) => t.id === currentId) ?? null,
    [library, plTracks, currentId],
  );

  // --- Playback ------------------------------------------------------------

  const onPlay = useCallback(
    async (id: number) => {
      queueRef.current = visibleTracks.map((t) => t.id);
      await playTrack(id);
      setCurrentId(id);
      setPaused(false);
      setPosMs(0);
    },
    [visibleTracks],
  );

  const playOffset = useCallback(
    async (delta: number) => {
      const q = queueRef.current;
      if (currentId == null || q.length === 0) return;
      const i = q.indexOf(currentId);
      const next = q[i + delta];
      if (next == null) {
        await stopPlayback();
        setCurrentId(null);
        setPosMs(0);
        return;
      }
      await playTrack(next);
      setCurrentId(next);
      setPaused(false);
      setPosMs(0);
    },
    [currentId],
  );

  // Auto-advance al terminar el track.
  useEffect(() => {
    if (current && !paused && current.durationMs > 0 && posMs >= current.durationMs - 600) {
      playOffset(1);
    }
  }, [posMs, current, paused, playOffset]);

  async function onToggle() {
    if (paused) {
      await resumePlayback();
      setPaused(false);
    } else {
      await pausePlayback();
      setPaused(true);
    }
  }

  async function onStop() {
    await stopPlayback();
    setCurrentId(null);
    setPaused(false);
    setPosMs(0);
  }

  async function onSeek(secs: number) {
    await seekTo(secs);
    setPosMs(secs * 1000);
  }

  // --- Import --------------------------------------------------------------

  async function onImport() {
    const folder = await open({ directory: true, multiple: false, title: 'Elegí tu carpeta de música' });
    if (!folder || typeof folder !== 'string') return;
    setBusy(true);
    setStatus('Importando…');
    try {
      const n = await importFolder(folder);
      setStatus(`Importados ${n} tracks.`);
      await refreshLibrary();
    } catch (e) {
      setStatus('Error import: ' + e);
    } finally {
      setBusy(false);
    }
  }

  // --- Organizacion --------------------------------------------------------

  async function onCreate(kind: NodeKind, parentId: number | null) {
    const name = kind === 'folder' ? 'Nueva carpeta' : 'Nueva playlist';
    const id = await createPlaylist(name, kind, parentId);
    await refreshPlaylists();
    if (kind === 'playlist') setSelection({ type: 'playlist', id });
  }

  async function onRename(id: number, name: string) {
    await renamePlaylist(id, name);
    await refreshPlaylists();
  }

  async function onDelete(id: number) {
    await deletePlaylist(id);
    if (selection.type === 'playlist' && selection.id === id) setSelection({ type: 'library' });
    await refreshPlaylists();
  }

  async function onMoveNode(id: number, parentId: number | null, index: number) {
    try {
      await movePlaylist(id, parentId, index);
    } catch (e) {
      setStatus(String(e));
    }
    await refreshPlaylists();
  }

  async function onDropTracks(playlistId: number, trackIds: number[]) {
    const n = await addTracksToPlaylist(playlistId, trackIds);
    setStatus(n > 0 ? `${n} agregados.` : 'Ya estaban en la playlist.');
    await Promise.all([refreshPlaylists(), refreshPlaylistTracks()]);
  }

  async function onReorder(trackIds: number[], index: number) {
    if (selection.type !== 'playlist') return;
    await reorderPlaylistTracks(selection.id, trackIds, index);
    await refreshPlaylistTracks();
  }

  async function onRemove(trackIds: number[]) {
    if (selection.type !== 'playlist') return;
    await removeTracksFromPlaylist(selection.id, trackIds);
    await Promise.all([refreshPlaylists(), refreshPlaylistTracks()]);
  }

  const selectedNode =
    selection.type === 'playlist' ? nodes.find((n) => n.id === selection.id) : null;

  return (
    <div className="app">
      <header>
        <h1>Sway</h1>
        <div className="search">
          <input
            type="search"
            placeholder="Buscar título, artista, álbum…"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
          />
        </div>
        <button className="primary" onClick={onImport} disabled={busy}>
          {busy ? 'Importando…' : '+ Importar carpeta'}
        </button>
      </header>

      <div className="body">
        <Sidebar
          nodes={nodes}
          selection={selection}
          onSelect={setSelection}
          onCreate={onCreate}
          onRename={onRename}
          onDelete={onDelete}
          onMoveNode={onMoveNode}
          onDropTracks={onDropTracks}
        />

        <main>
          <div className="view-head">
            <h2>{selection.type === 'library' ? 'Biblioteca' : selectedNode?.name ?? 'Playlist'}</h2>
            <span className="view-meta">
              {visibleTracks.length} tracks
              {selection.type === 'playlist' && !search && ' · arrastrá para ordenar · Supr quita'}
            </span>
            <span className="status">{status}</span>
          </div>
          {visibleTracks.length > 0 ? (
            <TrackTable
              tracks={visibleTracks}
              currentId={currentId}
              selected={selected}
              onSelectedChange={setSelected}
              onPlay={onPlay}
              canReorder={selection.type === 'playlist' && !search.trim()}
              onReorder={onReorder}
              onRemove={selection.type === 'playlist' ? onRemove : null}
            />
          ) : (
            <p className="empty">
              {search
                ? 'Nada coincide con la búsqueda.'
                : selection.type === 'library'
                  ? 'Importá una carpeta para empezar.'
                  : 'Arrastrá tracks desde la Biblioteca hasta esta playlist.'}
            </p>
          )}
        </main>
      </div>

      {current && (
        <PlayerBar
          track={current}
          paused={paused}
          posMs={posMs}
          onToggle={onToggle}
          onStop={onStop}
          onPrev={() => playOffset(-1)}
          onNext={() => playOffset(1)}
          onSeek={onSeek}
        />
      )}
    </div>
  );
}
