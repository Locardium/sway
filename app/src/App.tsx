import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Info, Menu, Settings as SettingsIcon } from 'lucide-react';
import { getCurrentWebview } from '@tauri-apps/api/webview';
import { listen } from '@tauri-apps/api/event';
import {
  Track,
  PlaylistNode,
  NodeKind,
  importFiles,
  listTracks,
  deleteTracks,
  trackPlaylists,
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
  subscribePlayback,
  setAppVisible,
  seekTo,
  setVolume as setVolumeBackend,
  revealTrack,
  syncXmlAfterChange,
} from './api';
import { beginDrag, didDrag, DragPayload, RawTarget } from './dnd';
import { isAndroid } from './platform';
import Sidebar, { Selection, NodeDropHint } from './components/Sidebar';
import TrackTable from './components/TrackTable';
import PlayerBar, { RepeatMode, REPEAT_LABEL } from './components/PlayerBar';
import RightPanel from './components/RightPanel';
import Settings from './components/Settings';
import { Modal, NamePrompt, Confirm } from './components/Modal';
import { MenuItem } from './components/ContextMenu';

type ModalState =
  | { type: 'name'; kind: NodeKind; parentId: number | null }
  | { type: 'confirm-node'; node: PlaylistNode }
  | { type: 'confirm-tracks'; ids: number[] }
  | { type: 'pick-playlist'; ids: number[] }
  | { type: 'track-playlists'; id: number; playlistIds: number[] }
  | { type: 'settings' }
  | null;

const VOL_STORAGE = 'sway.volume';
/// Antes de este punto del track, "atras" salta al anterior en vez de
/// reiniciar el actual.
const RESTART_MS = 3000;

function shuffled<T>(arr: T[]): T[] {
  const a = [...arr];
  for (let i = a.length - 1; i > 0; i--) {
    const j = Math.floor(Math.random() * (i + 1));
    [a[i], a[j]] = [a[j], a[i]];
  }
  return a;
}

export default function App() {
  const [library, setLibrary] = useState<Track[]>([]);
  const [nodes, setNodes] = useState<PlaylistNode[]>([]);
  const [selection, setSelection] = useState<Selection>({ type: 'library' });
  const [plTracks, setPlTracks] = useState<Track[]>([]);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [search, setSearch] = useState('');
  const [modal, setModal] = useState<ModalState>(null);
  const [infoOpen, setInfoOpen] = useState(false);
  const [infoId, setInfoId] = useState<number | null>(null);
  const [sidebarOpen, setSidebarOpen] = useState(false); // drawer en mobile
  const [importProgress, setImportProgress] = useState<{ done: number; total: number } | null>(null);

  // Hints de drop (drag interno + drag de archivos del OS).
  const [nodeDropHint, setNodeDropHint] = useState<NodeDropHint>(null);
  const [rootHover, setRootHover] = useState(false);
  const [dropInsertIndex, setDropInsertIndex] = useState<number | null>(null);

  const [currentId, setCurrentId] = useState<number | null>(null);
  const [paused, setPaused] = useState(false);
  const [posMs, setPosMs] = useState(0);
  const [shuffle, setShuffle] = useState(() => localStorage.getItem('sway.shuffle') === '1');
  const [repeat, setRepeat] = useState<RepeatMode>(() => {
    // 'all'/'one' son los nombres viejos (repeat de cola). El modo ahora es
    // siempre sobre el track actual, ver RepeatMode en PlayerBar.
    const saved = localStorage.getItem('sway.repeat');
    if (saved === 'track' || saved === 'once') return saved;
    if (saved === 'all' || saved === 'one') return 'track';
    return 'off';
  });
  const [volume, setVol] = useState(() => {
    const v = Number(localStorage.getItem(VOL_STORAGE));
    return isNaN(v) || v < 0 || v > 1 ? 1 : v;
  });
  const [status, setStatus] = useState('');
  const queueRef = useRef<number[]>([]);
  const volPending = useRef<number | null>(null);
  // Tras un seek, ignora el poll de posicion un rato (evita que la barra
  // salte al valor viejo antes de que el backend refleje la nueva posicion).
  const seekGuard = useRef(0);

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
        await setVolumeBackend(volume);
      } catch {
        if (tries++ < 5) setTimeout(attempt, 300);
      }
    };
    attempt();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    setSelected(new Set());
    setSearch('');
    if (selection.type === 'playlist') {
      playlistTracks(selection.id).then(setPlTracks).catch(() => setPlTracks([]));
    }
  }, [selection]);

  // Poll de posicion (desktop). Android no lo usa: el plugin nativo empuja su
  // estado, ver el efecto de abajo.
  useEffect(() => {
    if (subscribePlayback) return;
    const t = setInterval(async () => {
      if (currentId != null && !paused && Date.now() >= seekGuard.current) {
        try {
          setPosMs((await playbackPosition()) * 1000);
        } catch {}
      }
    }, 500);
    return () => clearInterval(t);
  }, [currentId, paused]);

  // Android: posicion, fin de track y play/pause llegan empujados por el
  // plugin en vez de por polling.
  useEffect(() => {
    if (!subscribePlayback) return;
    let stop: (() => void) | null = null;
    let dead = false;
    subscribePlayback((e) => {
      switch (e.type) {
        case 'position':
          if (Date.now() >= seekGuard.current) setPosMs(e.ms);
          break;
        case 'playing':
          setPaused(!e.value);
          break;
        case 'ended':
          onTrackEndedRef.current();
          break;
      }
    })
      .then((un) => {
        if (dead) un();
        else stop = un;
      })
      .catch(() => {});
    return () => {
      dead = true;
      stop?.();
    };
  }, []);

  // Botones de la notificacion (Android). Los manda MainActivity.kt, que
  // escucha los broadcasts de la notificacion del plugin — ver el comentario
  // largo ahi.
  useEffect(() => {
    const w = window as typeof window & {
      __swayMediaButton?: (button: string) => void;
      __swayAppVisible?: (visible: boolean) => void;
    };
    // MainActivity escucha por dos vias distintas (MediaSession y broadcast de
    // la notificacion) porque cual esta activa depende de la version de
    // Android y de la capa del fabricante. Si un mismo toque llega por las
    // dos, esto se queda con el primero.
    w.__swayAppVisible = (visible) => setAppVisible(visible);
    let lastAt = 0;
    w.__swayMediaButton = (button) => {
      const now = Date.now();
      if (now - lastAt < 400) return;
      lastAt = now;
      if (button === 'next') playOffsetRef.current(1);
      else if (button === 'prev') onPrev();
    };
    return () => {
      delete w.__swayMediaButton;
      delete w.__swayAppVisible;
    };
  }, []);

  // Progreso de importacion (copia a la carpeta gestionada).
  useEffect(() => {
    if (!('__TAURI_INTERNALS__' in window)) return;
    const un = listen<[number, number]>('import-progress', (e) => {
      const [done, total] = e.payload;
      setImportProgress({ done, total });
    });
    return () => {
      un.then((f) => f());
    };
  }, []);

  // La biblioteca cambió por fuera de esta pantalla: archivos nuevos que
  // encontró el watcher, o cambios que trajo el sync desde otro dispositivo.
  //
  // Hay que recargar TAMBIÉN la playlist abierta. Sin eso, el otro dispositivo
  // mueve o saca una canción, la DB local ya está bien, y la vista sigue
  // mostrando el orden viejo hasta que uno vuelve a hacer click en la
  // playlist.
  useEffect(() => {
    if (!('__TAURI_INTERNALS__' in window)) return;
    const un = listen('library-changed', () => {
      refreshLibrary();
      refreshPlaylists();
      refreshPlaylistTracks();
      setStatus('Library updated');
    });
    return () => {
      un.then((f) => f());
    };
  }, [refreshLibrary, refreshPlaylists, refreshPlaylistTracks]);

  // El status se auto-oculta (toast breve, no texto persistente).
  useEffect(() => {
    if (!status) return;
    const t = setTimeout(() => setStatus(''), 2600);
    return () => clearTimeout(t);
  }, [status]);

  // Barra espaciadora = play/pausa (salvo escribiendo o con un modal abierto).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.code !== 'Space' || currentId == null || modal) return;
      const el = e.target as HTMLElement;
      if (el.closest('input, textarea, [contenteditable="true"], [role="dialog"]')) return;
      e.preventDefault();
      onToggle();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [currentId, paused, modal]);

  // Gesto del drawer (solo donde el sidebar ES un drawer, mismo breakpoint que
  // el CSS): arrastrar hacia la derecha desde cualquier punto de la pantalla
  // lo abre, arrastrar hacia la izquierda con el abierto lo cierra. Nunca hace
  // preventDefault, asi que no pisa el scroll de la tabla.
  useEffect(() => {
    const TRIGGER_PX = 70; // recorrido horizontal minimo
    const SLOP_PX = 45; // recorrido vertical que cancela (es un scroll)
    // Controles que se manejan arrastrando en horizontal: ahi el gesto es
    // del control, no del drawer.
    const IGNORE = '.seek-bar, input[type="range"], [role="dialog"]';
    let startX = 0;
    let startY = 0;
    let armed: 'open' | 'close' | null = null;

    const onStart = (e: TouchEvent) => {
      armed = null;
      if (e.touches.length !== 1) return;
      if (!window.matchMedia('(max-width: 680px)').matches) return;
      if ((e.target as HTMLElement | null)?.closest?.(IGNORE)) return;
      const t = e.touches[0];
      startX = t.clientX;
      startY = t.clientY;
      armed = sidebarOpen ? 'close' : 'open';
    };
    const onMove = (e: TouchEvent) => {
      if (!armed || e.touches.length !== 1) return;
      const t = e.touches[0];
      const dx = t.clientX - startX;
      const dy = Math.abs(t.clientY - startY);
      // Con muy poco recorrido la direccion es puro ruido: recien pasada la
      // zona muerta tiene sentido decidir si es un gesto horizontal o un
      // scroll (si no, cualquier temblor inicial cancela el gesto).
      if (Math.hypot(dx, dy) < 12) return;
      if (dy > SLOP_PX || dy > Math.abs(dx)) {
        armed = null;
        return;
      }
      if (armed === 'open' && dx > TRIGGER_PX) {
        setSidebarOpen(true);
        armed = null;
      } else if (armed === 'close' && dx < -TRIGGER_PX) {
        setSidebarOpen(false);
        armed = null;
      }
    };
    const end = () => {
      armed = null;
    };
    window.addEventListener('touchstart', onStart, { passive: true });
    window.addEventListener('touchmove', onMove, { passive: true });
    window.addEventListener('touchend', end, { passive: true });
    window.addEventListener('touchcancel', end, { passive: true });
    return () => {
      window.removeEventListener('touchstart', onStart);
      window.removeEventListener('touchmove', onMove);
      window.removeEventListener('touchend', end);
      window.removeEventListener('touchcancel', end);
    };
  }, [sidebarOpen]);

  const searching = search.trim().length > 0;
  // Con búsqueda activa se busca SIEMPRE en toda la biblioteca, sin importar
  // la playlist seleccionada.
  const baseTracks = searching ? library : selection.type === 'library' ? library : plTracks;
  const visibleTracks = useMemo(() => {
    const q = search.trim().toLowerCase();
    if (!q) return baseTracks;
    return baseTracks.filter((t) =>
      [t.title, t.artist, t.album, t.genre].some((f) => f.toLowerCase().includes(q)),
    );
  }, [baseTracks, search]);

  const findTrack = useCallback(
    (id: number | null) =>
      id == null
        ? null
        : library.find((t) => t.id === id) ?? plTracks.find((t) => t.id === id) ?? null,
    [library, plTracks],
  );
  const current = useMemo(() => findTrack(currentId), [findTrack, currentId]);
  const infoTrack = useMemo(
    () => findTrack(infoId) ?? current,
    [findTrack, infoId, current],
  );

  // Mantiene el player montado durante su animacion de salida (al parar).
  const lastTrackRef = useRef<Track | null>(null);
  if (current) lastTrackRef.current = current;
  const [playerClosing, setPlayerClosing] = useState(false);
  useEffect(() => {
    if (current == null && lastTrackRef.current != null) {
      setPlayerClosing(true);
      const t = setTimeout(() => setPlayerClosing(false), 240);
      return () => clearTimeout(t);
    }
  }, [current]);
  const playerTrack = current ?? (playerClosing ? lastTrackRef.current : null);

  // --- Playback ------------------------------------------------------------

  function buildQueue(ids: number[], firstId: number, useShuffle: boolean): number[] {
    if (!useShuffle) return ids;
    return [firstId, ...shuffled(ids.filter((i) => i !== firstId))];
  }

  const onPlay = useCallback(
    async (id: number) => {
      try {
        queueRef.current = buildQueue(visibleTracks.map((t) => t.id), id, shuffle);
        await playTrack(id);
        setCurrentId(id);
        setPaused(false);
        setPosMs(0);
      } catch (e) {
        setStatus('Playback error: ' + e);
      }
    },
    [visibleTracks, shuffle],
  );

  const playOffset = useCallback(
    async (delta: number) => {
      const q = queueRef.current;
      if (currentId == null || q.length === 0) return;
      const next = q[q.indexOf(currentId) + delta];
      if (next == null) {
        // Borde de la cola. Nunca se suelta el track actual: si se limpiara
        // `currentId`, la app se quedaria sin nada seleccionado mientras el
        // player nativo sigue con el audio cargado — y desde ese estado
        // `playOffset` sale por el early return de arriba, o sea que ya no se
        // puede avanzar ni retroceder sin volver a elegir un track a mano.
        if (delta < 0) onSeek(0); // ya era el primero: vuelve a empezar
        else {
          await pausePlayback(); // fin de la cola: para, pero se queda ahi
          setPaused(true);
        }
        return;
      }
      await playTrack(next);
      setCurrentId(next);
      setPaused(false);
      setPosMs(0);
    },
    [currentId],
  );
  const playOffsetRef = useRef(playOffset);
  playOffsetRef.current = playOffset;
  const posMsRef = useRef(posMs);
  posMsRef.current = posMs;

  // Fin del track: el modo repeat manda sobre el track actual, no sobre la
  // cola. 'once' se apaga solo despues de repetir, asi la proxima vuelta
  // sigue de largo.
  //
  // El candado `advancing` esta porque el fin de track puede llegar mas de una
  // vez antes de que la posicion se resetee (en desktop se deduce de la
  // posicion, que sigue pasada de largo unos renders mas). Sin el, 'once'
  // apaga el repeat, el efecto vuelve a entrar con la posicion vieja y
  // saltea un track.
  const advancing = useRef(false);
  const onTrackEnded = useCallback(async () => {
    if (currentId == null || advancing.current) return;
    advancing.current = true;
    try {
      if (repeat === 'track' || repeat === 'once') {
        if (repeat === 'once') {
          setRepeat('off');
          localStorage.setItem('sway.repeat', 'off');
        }
        await playTrack(currentId);
        setPaused(false);
        setPosMs(0);
      } else {
        await playOffset(1);
      }
    } finally {
      advancing.current = false;
    }
  }, [currentId, repeat, playOffset]);
  const onTrackEndedRef = useRef(onTrackEnded);
  onTrackEndedRef.current = onTrackEnded;

  // Auto-advance en desktop: no hay evento de fin de track, se deduce de la
  // posicion. En Android lo dispara el plugin (ver el efecto de suscripcion).
  useEffect(() => {
    if (subscribePlayback) return;
    if (current && !paused && current.durationMs > 0 && posMs >= current.durationMs - 600) {
      onTrackEnded();
    }
  }, [posMs, current, paused, onTrackEnded]);

  function onToggleShuffle() {
    setShuffle((s) => {
      const next = !s;
      localStorage.setItem('sway.shuffle', next ? '1' : '0');
      // Rearma la cola manteniendo el track actual primero.
      if (currentId != null) {
        queueRef.current = next
          ? buildQueue(visibleTracks.map((t) => t.id), currentId, true)
          : visibleTracks.map((t) => t.id);
      }
      return next;
    });
  }

  function onCycleRepeat() {
    setRepeat((r) => {
      const next: RepeatMode = r === 'off' ? 'track' : r === 'track' ? 'once' : 'off';
      localStorage.setItem('sway.repeat', next);
      // En mobile no hay tooltip: el toast es la unica forma de saber en que
      // estado quedo el boton.
      setStatus(REPEAT_LABEL[next]);
      return next;
    });
  }

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

  // "Atras" estandar de cualquier reproductor: vuelve al principio del track,
  // y solo pasa al anterior si ya estabas en el principio. Lo comparten el
  // boton del player y el de la notificacion.
  function onPrev() {
    if (posMsRef.current > RESTART_MS) onSeek(0);
    else playOffsetRef.current(-1);
  }

  async function onSeek(secs: number) {
    seekGuard.current = Date.now() + 800;
    setPosMs(secs * 1000);
    await seekTo(secs);
  }

  // Volumen: UI responde al instante, el backend se actualiza con throttle.
  function onVolume(v: number) {
    setVol(v);
    const first = volPending.current == null;
    volPending.current = v;
    if (first) {
      setTimeout(() => {
        const val = volPending.current;
        volPending.current = null;
        if (val != null) {
          localStorage.setItem(VOL_STORAGE, String(val));
          setVolumeBackend(val).catch(() => {});
        }
      }, 60);
    }
  }

  // --- Drop de archivos del OS --------------------------------------------

  const osDropRef = useRef<(paths: string[], x: number, y: number) => void>(() => {});
  osDropRef.current = async (paths, x, y) => {
    const el = document.elementFromPoint(x, y)?.closest<HTMLElement>('[data-dnd="node"]');
    const nodeId = el ? Number(el.dataset.nodeId) : null;
    const nodeKind = el?.dataset.nodeKind ?? null;
    setStatus('Importing…');
    try {
      const ids = await importFiles(paths);
      if (ids.length === 0) {
        setStatus('No audio files in what you dropped.');
        return;
      }
      if (nodeId != null && nodeKind === 'playlist') {
        const n = await addTracksToPlaylist(nodeId, ids);
        setStatus(`Imported ${ids.length}, added ${n} to the playlist.`);
      } else if (nodeId != null && nodeKind === 'folder') {
        const isSingleDir = paths.length === 1;
        const base = paths[0].replace(/[\\/]+$/, '').split(/[\\/]/).pop() ?? 'Importados';
        if (isSingleDir) {
          const pid = await createPlaylist(base, 'playlist', nodeId);
          await addTracksToPlaylist(pid, ids);
          setStatus(`Playlist "${base}" created with ${ids.length} tracks.`);
          setSelection({ type: 'playlist', id: pid });
        } else {
          setStatus(`Imported ${ids.length} tracks to the Library.`);
        }
      } else if (selection.type === 'playlist') {
        const n = await addTracksToPlaylist(selection.id, ids);
        setStatus(`Imported ${ids.length}, added ${n} to the playlist.`);
      } else {
        setStatus(`Imported ${ids.length} tracks to the Library.`);
      }
      await Promise.all([refreshLibrary(), refreshPlaylists(), refreshPlaylistTracks()]);
      syncXmlAfterChange().catch(() => {});
    } catch (e) {
      setStatus('Import error: ' + e);
    } finally {
      // Mantiene el 100% visible un momento antes de ocultar el toast.
      setTimeout(() => setImportProgress(null), 1000);
    }
  };

  useEffect(() => {
    // Solo dentro de Tauri (en un browser normal no hay drop de archivos del OS).
    if (!('__TAURI_INTERNALS__' in window)) return;
    const unlisten = getCurrentWebview().onDragDropEvent((event) => {
      const p = event.payload;
      const scale = window.devicePixelRatio || 1;
      if (p.type === 'over' || p.type === 'enter') {
        const el = document
          .elementFromPoint(p.position.x / scale, p.position.y / scale)
          ?.closest<HTMLElement>('[data-dnd="node"][data-node-kind]');
        setNodeDropHint(
          el ? { nodeId: Number(el.dataset.nodeId), zone: 'into' } : null,
        );
      } else if (p.type === 'drop') {
        setNodeDropHint(null);
        osDropRef.current(p.paths, p.position.x / scale, p.position.y / scale);
      } else {
        setNodeDropHint(null);
      }
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  // --- Drag interno (tracks y nodos) ---------------------------------------

  function clearDropHints() {
    setNodeDropHint(null);
    setRootHover(false);
    setDropInsertIndex(null);
  }

  const canReorder = selection.type === 'playlist' && !search.trim();

  function onDragHover(payload: DragPayload, target: RawTarget) {
    if (payload.kind === 'tracks') {
      if (target?.type === 'node' && target.nodeKind === 'playlist') {
        setNodeDropHint({ nodeId: target.id, zone: 'into' });
        setDropInsertIndex(null);
      } else if (target?.type === 'insert' && canReorder) {
        setDropInsertIndex(target.index);
        setNodeDropHint(null);
      } else {
        clearDropHints();
      }
      return;
    }
    // payload nodo
    if (target?.type === 'node' && target.id !== payload.id) {
      setNodeDropHint({ nodeId: target.id, zone: target.zone });
      setRootHover(false);
    } else if (target?.type === 'root') {
      setRootHover(true);
      setNodeDropHint(null);
    } else {
      clearDropHints();
    }
  }

  async function onDragDrop(payload: DragPayload, target: RawTarget) {
    clearDropHints();
    if (payload.kind === 'tracks') {
      if (target?.type === 'node' && target.nodeKind === 'playlist') {
        await onDropTracks(target.id, payload.ids);
      } else if (target?.type === 'insert' && canReorder) {
        await onReorder(payload.ids, target.index);
      }
      return;
    }
    const childCount = (pid: number | null) => nodes.filter((n) => n.parentId === pid).length;
    if (target?.type === 'node' && target.id !== payload.id) {
      const tNode = nodes.find((n) => n.id === target.id);
      if (!tNode) return;
      if (target.zone === 'into') {
        await onMoveNode(payload.id, target.id, childCount(target.id));
      } else {
        const siblings = nodes
          .filter((n) => n.parentId === tNode.parentId)
          .sort((a, b) => a.position - b.position);
        let idx = siblings.findIndex((s) => s.id === target.id);
        if (target.zone === 'after') idx += 1;
        await onMoveNode(payload.id, tNode.parentId, idx);
      }
    } else if (target?.type === 'root') {
      await onMoveNode(payload.id, null, childCount(null));
    }
  }

  function onTrackMouseDown(e: React.MouseEvent, ids: number[]) {
    const label = ids.length === 1 ? findTrack(ids[0])?.title ?? '1 track' : `${ids.length} tracks`;
    beginDrag(e, { kind: 'tracks', ids, label }, {
      onHover: onDragHover,
      onDrop: onDragDrop,
      onEnd: clearDropHints,
    });
  }

  function onNodeMouseDown(e: React.MouseEvent, id: number) {
    const label = nodes.find((n) => n.id === id)?.name ?? '';
    beginDrag(e, { kind: 'node', id, label }, {
      onHover: onDragHover,
      onDrop: onDragDrop,
      onEnd: clearDropHints,
    });
  }

  // --- Organizacion --------------------------------------------------------

  async function doCreate(name: string, kind: NodeKind, parentId: number | null) {
    const id = await createPlaylist(name, kind, parentId);
    await refreshPlaylists();
    if (kind === 'playlist') setSelection({ type: 'playlist', id });
    syncXmlAfterChange().catch(() => {});
  }

  async function onRename(id: number, name: string) {
    await renamePlaylist(id, name);
    await refreshPlaylists();
    syncXmlAfterChange().catch(() => {});
  }

  async function doDeleteNode(id: number) {
    await deletePlaylist(id);
    if (selection.type === 'playlist' && selection.id === id) setSelection({ type: 'library' });
    await refreshPlaylists();
    syncXmlAfterChange().catch(() => {});
  }

  async function onMoveNode(id: number, parentId: number | null, index: number) {
    try {
      await movePlaylist(id, parentId, index);
      syncXmlAfterChange().catch(() => {});
    } catch (e) {
      setStatus(String(e));
    }
    await refreshPlaylists();
  }

  async function onDropTracks(playlistId: number, trackIds: number[]) {
    const n = await addTracksToPlaylist(playlistId, trackIds);
    setStatus(n > 0 ? `Added ${n}.` : 'Already in the playlist.');
    await Promise.all([refreshPlaylists(), refreshPlaylistTracks()]);
    syncXmlAfterChange().catch(() => {});
  }

  async function onReorder(trackIds: number[], index: number) {
    if (selection.type !== 'playlist') return;
    await reorderPlaylistTracks(selection.id, trackIds, index);
    await refreshPlaylistTracks();
    syncXmlAfterChange().catch(() => {});
  }

  async function doRemoveFromPlaylist(trackIds: number[]) {
    if (selection.type !== 'playlist') return;
    await removeTracksFromPlaylist(selection.id, trackIds);
    setSelected(new Set());
    await Promise.all([refreshPlaylists(), refreshPlaylistTracks()]);
    syncXmlAfterChange().catch(() => {});
  }

  async function doDeleteTracks(ids: number[]) {
    await deleteTracks(ids);
    if (currentId != null && ids.includes(currentId)) await onStop();
    setSelected(new Set());
    setStatus(`Removed ${ids.length} from the library.`);
    await Promise.all([refreshLibrary(), refreshPlaylists(), refreshPlaylistTracks()]);
    syncXmlAfterChange().catch(() => {});
  }

  function rowMenuItems(ids: number[]): MenuItem[] {
    const n = ids.length;
    const viewIsPlaylist = selection.type === 'playlist' && !searching;
    const items: MenuItem[] = [
      { label: 'Play', disabled: n !== 1, onClick: () => onPlay(ids[0]) },
      { label: 'Add to playlist…', onClick: () => setModal({ type: 'pick-playlist', ids }) },
      {
        label: 'Show playlists',
        disabled: n !== 1,
        onClick: async () => {
          const playlistIds = await trackPlaylists(ids[0]);
          setModal({ type: 'track-playlists', id: ids[0], playlistIds });
        },
      },
      ...(isAndroid()
        ? []
        : [
            {
              label: 'Reveal in Explorer',
              disabled: n !== 1,
              onClick: () => revealTrack(ids[0]).catch((e: unknown) => setStatus(String(e))),
            } satisfies MenuItem,
          ]),
      { separator: true, label: '' },
    ];
    if (viewIsPlaylist) {
      items.push({
        label: n === 1 ? 'Remove from playlist' : `Remove ${n} from playlist`,
        danger: true,
        onClick: () => doRemoveFromPlaylist(ids),
      });
    }
    items.push({
      label: n === 1 ? 'Delete from library…' : `Delete ${n} from library…`,
      danger: true,
      onClick: () => setModal({ type: 'confirm-tracks', ids }),
    });
    return items;
  }

  // Wrappers referencialmente estables para la tabla: su identidad no cambia
  // entre renders, asi React.memo evita re-renderizar las ~1149 filas cuando
  // solo cambia el estado del player (posicion, pausa, volumen).
  const tableApi = useRef({ onPlay, onTrackMouseDown, rowMenuItems });
  tableApi.current = { onPlay, onTrackMouseDown, rowMenuItems };
  const stableOnPlay = useCallback((id: number) => tableApi.current.onPlay(id), []);
  const stableOnTrackMouseDown = useCallback(
    (e: React.MouseEvent, ids: number[]) => tableApi.current.onTrackMouseDown(e, ids),
    [],
  );
  const stableRowMenuItems = useCallback(
    (ids: number[]) => tableApi.current.rowMenuItems(ids),
    [],
  );
  const stableWasDrag = useCallback(() => didDrag, []);

  const selectedNode =
    selection.type === 'playlist' ? nodes.find((n) => n.id === selection.id) : null;

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
        <div className="header-left">
          <button
            className="mini burger"
            title="Menu"
            onClick={() => setSidebarOpen((o) => !o)}
          >
            <Menu size={18} />
          </button>
          <h1>Sway</h1>
        </div>
        <div className="search">
          <input
            type="search"
            placeholder="Search your library…"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
          />
        </div>
        <div className="header-right">
          <button
            className={'mini' + (infoOpen ? ' on' : '')}
            title="Track info"
            onClick={() => setInfoOpen((o) => !o)}
          >
            <Info size={17} />
          </button>
          <button
            className="mini gear"
            title="Settings"
            onClick={() => setModal({ type: 'settings' })}
          >
            <SettingsIcon size={17} />
          </button>
        </div>
      </header>

      <div className={'body' + (sidebarOpen ? ' sidebar-open' : '')}>
        {sidebarOpen && <div className="sidebar-scrim" onClick={() => setSidebarOpen(false)} />}
        <Sidebar
          nodes={nodes}
          selection={selection}
          dropHint={nodeDropHint}
          rootHover={rootHover}
          onSelect={(sel) => {
            setSelection(sel);
            setSidebarOpen(false);
          }}
          onCreate={(kind, parentId) => setModal({ type: 'name', kind, parentId })}
          onRename={onRename}
          onDelete={(id) => {
            const node = nodes.find((n) => n.id === id);
            if (node) setModal({ type: 'confirm-node', node });
          }}
          onNodeMouseDown={onNodeMouseDown}
          wasDrag={() => didDrag}
        />

        <main>
          <div className="view-head">
            <h2>
              {searching || selection.type === 'library'
                ? 'Library'
                : selectedNode?.name ?? 'Playlist'}
            </h2>
            <span className="view-meta">
              {visibleTracks.length} tracks
              {searching && ' · searching the whole library'}
              {canReorder && ' · drag to reorder'}
            </span>
          </div>
          {visibleTracks.length > 0 ? (
            <TrackTable
              tracks={visibleTracks}
              currentId={currentId}
              selected={selected}
              onSelectedChange={setSelected}
              onPlay={stableOnPlay}
              onInspect={setInfoId}
              canReorder={canReorder}
              onTrackMouseDown={stableOnTrackMouseDown}
              dropInsertIndex={dropInsertIndex}
              wasDrag={stableWasDrag}
              rowMenuItems={stableRowMenuItems}
              tapToPlay={isAndroid()}
            />
          ) : (
            <p className="empty">
              {search
                ? 'Nothing matches your search.'
                : isAndroid()
                  ? 'No tracks yet. Import tracks from Settings → Library.'
                  : selection.type === 'library'
                    ? 'Drag music or folders from your computer to get started.'
                    : 'Drag tracks from the Library or from your computer here.'}
            </p>
          )}
        </main>

        {infoOpen && <div className="rp-scrim" onClick={() => setInfoOpen(false)} />}
        <RightPanel
          open={infoOpen}
          track={infoTrack}
          isPlaying={infoTrack != null && infoTrack.id === currentId}
          onClose={() => setInfoOpen(false)}
        />
      </div>

      {playerTrack && (
        <PlayerBar
          track={playerTrack}
          closing={current == null}
          paused={paused}
          posMs={posMs}
          volume={volume}
          shuffle={shuffle}
          repeat={repeat}
          onToggle={onToggle}
          onStop={onStop}
          onPrev={onPrev}
          onNext={() => playOffset(1)}
          onSeek={onSeek}
          onVolume={onVolume}
          onToggleShuffle={onToggleShuffle}
          onCycleRepeat={onCycleRepeat}
          showVolume={!isAndroid()}
        />
      )}

      {modal?.type === 'name' && (
        <NamePrompt
          title={modal.kind === 'folder' ? 'New folder' : 'New playlist'}
          placeholder={modal.kind === 'folder' ? 'Folder name' : 'Playlist name'}
          submitLabel="Create"
          onSubmit={(name) => doCreate(name, modal.kind, modal.parentId)}
          onClose={() => setModal(null)}
        />
      )}
      {modal?.type === 'confirm-node' && (
        <Confirm
          title={modal.node.kind === 'folder' ? 'Delete folder' : 'Delete playlist'}
          message={
            modal.node.kind === 'folder'
              ? `"${modal.node.name}" and everything inside it are deleted. The tracks stay in your Library.`
              : `"${modal.node.name}" is deleted. The tracks stay in your Library.`
          }
          confirmLabel="Delete"
          onConfirm={() => doDeleteNode(modal.node.id)}
          onClose={() => setModal(null)}
        />
      )}
      {modal?.type === 'confirm-tracks' && (
        <Confirm
          title="Delete from library"
          message={`${modal.ids.length} track(s) are removed from the library and all playlists. Files managed by Sway go to the OS trash; others are left on disk.`}
          confirmLabel="Delete"
          onConfirm={() => doDeleteTracks(modal.ids)}
          onClose={() => setModal(null)}
        />
      )}
      {modal?.type === 'pick-playlist' && (
        <Modal title="Add to playlist" onClose={() => setModal(null)}>
          {playlistOptions.length > 0 ? (
            <div className="pick-list">
              {playlistOptions.map((p) => (
                <button
                  key={p.id}
                  onClick={async () => {
                    const n = await addTracksToPlaylist(p.id, modal.ids);
                    setStatus(n > 0 ? `Added ${n}.` : 'Already in the playlist.');
                    await Promise.all([refreshPlaylists(), refreshPlaylistTracks()]);
                    syncXmlAfterChange().catch(() => {});
                    setModal(null);
                  }}
                >
                  {p.label}
                </button>
              ))}
            </div>
          ) : (
            <p className="modal-msg">No playlists yet. Create one from the sidebar.</p>
          )}
        </Modal>
      )}
      {modal?.type === 'track-playlists' && (
        <Modal title="In playlists" onClose={() => setModal(null)}>
          {modal.playlistIds.length > 0 ? (
            <div className="pick-list">
              {modal.playlistIds.map((pid) => {
                const label = playlistOptions.find((p) => p.id === pid)?.label ?? `#${pid}`;
                return (
                  <button
                    key={pid}
                    onClick={() => {
                      setSelection({ type: 'playlist', id: pid });
                      setSearch('');
                      setModal(null);
                    }}
                  >
                    {label}
                  </button>
                );
              })}
            </div>
          ) : (
            <p className="modal-msg">Not in any playlist.</p>
          )}
        </Modal>
      )}
      {modal?.type === 'settings' && (
        <Settings
          trackCount={library.length}
          volume={volume}
          onClose={() => setModal(null)}
          onStatus={setStatus}
          onImported={async () => {
            await Promise.all([refreshLibrary(), refreshPlaylists()]);
          }}
        />
      )}

      {status && (
        <div className="status-toast" role="status">
          {status}
        </div>
      )}

      {importProgress && (
        <div className="import-toast" role="status">
          <div className="import-toast-head">
            <span>Copying to your library…</span>
            <span className="import-toast-count">
              {importProgress.done} / {importProgress.total}
            </span>
          </div>
          <div className="import-bar">
            <div
              className="import-bar-fill"
              style={{ width: `${(importProgress.done / Math.max(1, importProgress.total)) * 100}%` }}
            />
          </div>
        </div>
      )}
    </div>
  );
}
