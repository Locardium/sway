import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { open } from '@tauri-apps/plugin-dialog';
import { getCurrentWebview } from '@tauri-apps/api/webview';
import {
  Track,
  PlaylistNode,
  NodeKind,
  importFolder,
  importFiles,
  listTracks,
  deleteTracks,
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
  setVolume as setVolumeBackend,
} from './api';
import Sidebar, { Selection } from './components/Sidebar';
import TrackTable from './components/TrackTable';
import PlayerBar from './components/PlayerBar';
import RightPanel from './components/RightPanel';
import { Modal, NamePrompt, Confirm } from './components/Modal';
import { MenuItem } from './components/ContextMenu';

type ModalState =
  | { type: 'name'; kind: NodeKind; parentId: number | null }
  | { type: 'confirm-node'; node: PlaylistNode }
  | { type: 'confirm-tracks'; ids: number[] }
  | { type: 'pick-playlist'; ids: number[] }
  | { type: 'settings' }
  | null;

const VOL_STORAGE = 'sway.volume';

export default function App() {
  const [library, setLibrary] = useState<Track[]>([]);
  const [nodes, setNodes] = useState<PlaylistNode[]>([]);
  const [selection, setSelection] = useState<Selection>({ type: 'library' });
  const [plTracks, setPlTracks] = useState<Track[]>([]);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [search, setSearch] = useState('');
  const [modal, setModal] = useState<ModalState>(null);
  const [infoOpen, setInfoOpen] = useState(false);
  const [osDropNodeId, setOsDropNodeId] = useState<number | null>(null);

  const [currentId, setCurrentId] = useState<number | null>(null);
  const [paused, setPaused] = useState(false);
  const [posMs, setPosMs] = useState(0);
  const [volume, setVol] = useState(() => {
    const v = Number(localStorage.getItem(VOL_STORAGE));
    return isNaN(v) || v <= 0 || v > 1 ? 1 : v;
  });
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
        if (volume !== 1) await setVolumeBackend(volume);
      } catch {
        if (tries++ < 5) setTimeout(attempt, 300);
      }
    };
    attempt();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

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
      const next = q[q.indexOf(currentId) + delta];
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

  async function onVolume(v: number) {
    setVol(v);
    localStorage.setItem(VOL_STORAGE, String(v));
    await setVolumeBackend(v);
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

  // --- Drop de archivos del OS --------------------------------------------

  const osDropRef = useRef<(paths: string[], x: number, y: number) => void>(() => {});
  osDropRef.current = async (paths, x, y) => {
    const el = document.elementFromPoint(x, y)?.closest<HTMLElement>('[data-drop-node]');
    const nodeId = el ? Number(el.dataset.dropNode) : null;
    const nodeKind = el?.dataset.nodeKind ?? null;
    setBusy(true);
    setStatus('Importando…');
    try {
      const ids = await importFiles(paths);
      if (ids.length === 0) {
        setStatus('Nada de audio en lo que soltaste.');
        return;
      }
      if (nodeId != null && nodeKind === 'playlist') {
        const n = await addTracksToPlaylist(nodeId, ids);
        setStatus(`${ids.length} importados, ${n} agregados a la playlist.`);
      } else if (nodeId != null && nodeKind === 'folder') {
        // Un directorio solo sobre una carpeta: crea playlist con su nombre.
        const isSingleDir = paths.length === 1;
        const base = paths[0].replace(/[\\/]+$/, '').split(/[\\/]/).pop() ?? 'Importados';
        if (isSingleDir) {
          const pid = await createPlaylist(base, 'playlist', nodeId);
          await addTracksToPlaylist(pid, ids);
          setStatus(`Playlist «${base}» creada con ${ids.length} tracks.`);
          setSelection({ type: 'playlist', id: pid });
        } else {
          setStatus(`${ids.length} tracks importados a la Biblioteca.`);
        }
      } else if (selection.type === 'playlist') {
        const n = await addTracksToPlaylist(selection.id, ids);
        setStatus(`${ids.length} importados, ${n} agregados a la playlist.`);
      } else {
        setStatus(`${ids.length} tracks importados a la Biblioteca.`);
      }
      await Promise.all([refreshLibrary(), refreshPlaylists(), refreshPlaylistTracks()]);
    } catch (e) {
      setStatus('Error import: ' + e);
    } finally {
      setBusy(false);
    }
  };

  useEffect(() => {
    // Solo dentro de Tauri (en un browser normal no hay drop de archivos del OS).
    if (!('__TAURI_INTERNALS__' in window)) return;
    const unlisten = getCurrentWebview().onDragDropEvent((event) => {
      const p = event.payload;
      if (p.type === 'over' || p.type === 'enter') {
        const scale = window.devicePixelRatio || 1;
        const el = document
          .elementFromPoint(p.position.x / scale, p.position.y / scale)
          ?.closest<HTMLElement>('[data-drop-node]');
        setOsDropNodeId(el ? Number(el.dataset.dropNode) : null);
      } else if (p.type === 'drop') {
        setOsDropNodeId(null);
        const scale = window.devicePixelRatio || 1;
        osDropRef.current(p.paths, p.position.x / scale, p.position.y / scale);
      } else {
        setOsDropNodeId(null);
      }
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  // --- Organizacion --------------------------------------------------------

  async function doCreate(name: string, kind: NodeKind, parentId: number | null) {
    const id = await createPlaylist(name, kind, parentId);
    await refreshPlaylists();
    if (kind === 'playlist') setSelection({ type: 'playlist', id });
  }

  async function onRename(id: number, name: string) {
    await renamePlaylist(id, name);
    await refreshPlaylists();
  }

  async function doDeleteNode(id: number) {
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

  async function doRemoveFromPlaylist(trackIds: number[]) {
    if (selection.type !== 'playlist') return;
    await removeTracksFromPlaylist(selection.id, trackIds);
    setSelected(new Set());
    await Promise.all([refreshPlaylists(), refreshPlaylistTracks()]);
  }

  async function doDeleteTracks(ids: number[]) {
    await deleteTracks(ids);
    if (currentId != null && ids.includes(currentId)) await onStop();
    setSelected(new Set());
    setStatus(`${ids.length} eliminados de la biblioteca.`);
    await Promise.all([refreshLibrary(), refreshPlaylists(), refreshPlaylistTracks()]);
  }

  // Menu contextual de filas: lo arma App porque depende de la vista.
  function rowMenuItems(ids: number[]): MenuItem[] {
    const n = ids.length;
    const items: MenuItem[] = [
      { label: 'Reproducir', disabled: n !== 1, onClick: () => onPlay(ids[0]) },
      { label: 'Agregar a playlist…', onClick: () => setModal({ type: 'pick-playlist', ids }) },
      { separator: true, label: '' },
    ];
    if (selection.type === 'playlist') {
      items.push({
        label: n === 1 ? 'Quitar de la playlist' : `Quitar ${n} de la playlist`,
        danger: true,
        onClick: () => doRemoveFromPlaylist(ids),
      });
    }
    items.push({
      label: n === 1 ? 'Eliminar de la biblioteca…' : `Eliminar ${n} de la biblioteca…`,
      danger: true,
      onClick: () => setModal({ type: 'confirm-tracks', ids }),
    });
    return items;
  }

  const selectedNode =
    selection.type === 'playlist' ? nodes.find((n) => n.id === selection.id) : null;

  // Lista plana de playlists (con ruta de carpetas) para el picker.
  const playlistOptions = useMemo(() => {
    const path = (n: PlaylistNode): string => {
      const parent = nodes.find((x) => x.id === n.parentId);
      return parent ? path(parent) + ' / ' + n.name : n.name;
    };
    return nodes.filter((n) => n.kind === 'playlist').map((n) => ({ id: n.id, label: path(n) }));
  }, [nodes]);

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
        <button
          className="mini gear"
          title="Configuración"
          onClick={() => setModal({ type: 'settings' })}
        >
          ⚙
        </button>
      </header>

      <div className="body">
        <Sidebar
          nodes={nodes}
          selection={selection}
          osDropNodeId={osDropNodeId}
          busy={busy}
          onSelect={setSelection}
          onImport={onImport}
          onCreate={(kind, parentId) => setModal({ type: 'name', kind, parentId })}
          onRename={onRename}
          onDelete={(id) => {
            const node = nodes.find((n) => n.id === id);
            if (node) setModal({ type: 'confirm-node', node });
          }}
          onMoveNode={onMoveNode}
          onDropTracks={onDropTracks}
        />

        <main>
          <div className="view-head">
            <h2>{selection.type === 'library' ? 'Biblioteca' : selectedNode?.name ?? 'Playlist'}</h2>
            <span className="view-meta">
              {visibleTracks.length} tracks
              {selection.type === 'playlist' && !search && ' · arrastrá para ordenar'}
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
              rowMenuItems={rowMenuItems}
            />
          ) : (
            <p className="empty">
              {search
                ? 'Nada coincide con la búsqueda.'
                : selection.type === 'library'
                  ? 'Importá una carpeta, o arrastrá música desde tu PC.'
                  : 'Arrastrá tracks desde la Biblioteca o desde tu PC hasta acá.'}
            </p>
          )}
        </main>

        <RightPanel open={infoOpen} track={current} onClose={() => setInfoOpen(false)} />
      </div>

      {current && (
        <PlayerBar
          track={current}
          paused={paused}
          posMs={posMs}
          volume={volume}
          infoOpen={infoOpen}
          onToggle={onToggle}
          onStop={onStop}
          onPrev={() => playOffset(-1)}
          onNext={() => playOffset(1)}
          onSeek={onSeek}
          onVolume={onVolume}
          onToggleInfo={() => setInfoOpen((o) => !o)}
        />
      )}

      {modal?.type === 'name' && (
        <NamePrompt
          title={modal.kind === 'folder' ? 'Nueva carpeta' : 'Nueva playlist'}
          placeholder={modal.kind === 'folder' ? 'Nombre de la carpeta' : 'Nombre de la playlist'}
          submitLabel="Crear"
          onSubmit={(name) => doCreate(name, modal.kind, modal.parentId)}
          onClose={() => setModal(null)}
        />
      )}
      {modal?.type === 'confirm-node' && (
        <Confirm
          title={modal.node.kind === 'folder' ? 'Eliminar carpeta' : 'Eliminar playlist'}
          message={
            modal.node.kind === 'folder'
              ? `«${modal.node.name}» y todo su contenido se eliminan. Los tracks siguen en la Biblioteca.`
              : `«${modal.node.name}» se elimina. Los tracks siguen en la Biblioteca.`
          }
          confirmLabel="Eliminar"
          onConfirm={() => doDeleteNode(modal.node.id)}
          onClose={() => setModal(null)}
        />
      )}
      {modal?.type === 'confirm-tracks' && (
        <Confirm
          title="Eliminar de la biblioteca"
          message={`${modal.ids.length} track(s) se eliminan de la biblioteca y de todas las playlists. Los archivos en disco no se tocan.`}
          confirmLabel="Eliminar"
          onConfirm={() => doDeleteTracks(modal.ids)}
          onClose={() => setModal(null)}
        />
      )}
      {modal?.type === 'pick-playlist' && (
        <Modal title="Agregar a playlist" onClose={() => setModal(null)}>
          {playlistOptions.length > 0 ? (
            <div className="pick-list">
              {playlistOptions.map((p) => (
                <button
                  key={p.id}
                  onClick={async () => {
                    const n = await addTracksToPlaylist(p.id, modal.ids);
                    setStatus(n > 0 ? `${n} agregados.` : 'Ya estaban en la playlist.');
                    await Promise.all([refreshPlaylists(), refreshPlaylistTracks()]);
                    setModal(null);
                  }}
                >
                  ♪ {p.label}
                </button>
              ))}
            </div>
          ) : (
            <p className="modal-msg">No hay playlists todavía. Creá una desde la barra lateral.</p>
          )}
        </Modal>
      )}
      {modal?.type === 'settings' && (
        <Modal title="Configuración" onClose={() => setModal(null)}>
          <dl className="settings-list">
            <dt>Versión</dt>
            <dd>Sway 0.1.0</dd>
            <dt>Biblioteca</dt>
            <dd>{library.length} tracks</dd>
            <dt>Volumen</dt>
            <dd>{Math.round(volume * 100)}%</dd>
          </dl>
          <p className="modal-msg muted">
            Export a Rekordbox/Serato y más opciones llegan en la próxima fase.
          </p>
        </Modal>
      )}
    </div>
  );
}
