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
  playbackState,
  setNextTrack,
  getPlaybackPrefs,
  setPlaybackPrefs,
  setTrackGain,
  subscribePlayback,
  setAppVisible,
  seekTo,
  setVolume as setVolumeBackend,
  revealTrack,
  syncXmlAfterChange,
  watchConditions,
  type PlaybackPrefs,
} from './api';
import { beginDrag, didDrag, DragPayload, RawTarget } from './dnd';
import { isAndroid } from './platform';
import Sidebar, { Selection, NodeDropHint } from './components/Sidebar';
import TrackTable from './components/TrackTable';
import PlayerBar, { RepeatMode, REPEAT_LABEL } from './components/PlayerBar';
import RightPanel from './components/RightPanel';
import Settings from './components/Settings';
import Sync from './components/Sync';
import { Modal, NamePrompt, Confirm } from './components/Modal';
import { MenuItem } from './components/ContextMenu';

type ModalState =
  | { type: 'name'; kind: NodeKind; parentId: number | null }
  | { type: 'confirm-node'; node: PlaylistNode }
  | { type: 'confirm-tracks'; ids: number[] }
  | { type: 'pick-playlist'; ids: number[] }
  | { type: 'track-playlists'; id: number; playlistIds: number[] }
  | { type: 'settings' }
  | { type: 'sync' }
  | null;

const VOL_STORAGE = 'sway.volume';
/// Before this point in the track, "back" jumps to the previous one instead
/// of restarting the current one.
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
  const [sidebarOpen, setSidebarOpen] = useState(false); // drawer on mobile
  const [importProgress, setImportProgress] = useState<{ done: number; total: number } | null>(null);

  // Drop hints (internal drag + OS file drag).
  const [nodeDropHint, setNodeDropHint] = useState<NodeDropHint>(null);
  const [rootHover, setRootHover] = useState(false);
  const [dropInsertIndex, setDropInsertIndex] = useState<number | null>(null);

  const [currentId, setCurrentId] = useState<number | null>(null);
  const [paused, setPaused] = useState(false);
  const [posMs, setPosMs] = useState(0);
  const [shuffle, setShuffle] = useState(() => localStorage.getItem('sway.shuffle') === '1');
  const [repeat, setRepeat] = useState<RepeatMode>(() => {
    // 'all'/'one' are the old names (queue repeat). The mode is now always
    // about the current track, see RepeatMode in PlayerBar.
    const saved = localStorage.getItem('sway.repeat');
    if (saved === 'track' || saved === 'once') return saved;
    if (saved === 'all' || saved === 'one') return 'track';
    return 'off';
  });
  const [volume, setVol] = useState(() => {
    const v = Number(localStorage.getItem(VOL_STORAGE));
    return isNaN(v) || v < 0 || v > 1 ? 1 : v;
  });
  // Playback preferences live in the backend (the audio thread needs them
  // anyway), and this is the copy the screens read. Defaults match
  // `db::PlaybackPrefs::default` so the UI isn't blank for the one frame
  // before the real values land.
  const [prefs, setPrefs] = useState<PlaybackPrefs>({
    crossfadeSecs: 0,
    gapless: true,
    autoplay: true,
    normalize: false,
    outputDevice: null,
  });
  const [status, setStatus] = useState('');
  const queueRef = useRef<number[]>([]);
  const volPending = useRef<number | null>(null);
  // After a seek, ignore the position poll for a bit (prevents the bar from
  // jumping back to the old value before the backend reflects the new position).
  const seekGuard = useRef(0);

  const refreshLibrary = useCallback(async () => {
    const tracks = await listTracks();
    setLibrary(tracks);
    return tracks;
  }, []);
  const refreshPlaylists = useCallback(async () => {
    setNodes(await listPlaylists());
  }, []);
  const refreshPlaylistTracks = useCallback(async () => {
    if (selection.type === 'playlist') setPlTracks(await playlistTracks(selection.id));
  }, [selection]);

  // Initial load with retries (the backend may take a while to register its state).
  useEffect(() => {
    let tries = 0;
    const attempt = async () => {
      try {
        await Promise.all([refreshLibrary(), refreshPlaylists()]);
        await setVolumeBackend(volume);
        setPrefs(await getPlaybackPrefs());
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

  // State poll (desktop). Android doesn't use it: the native plugin pushes
  // its state, see the effect below.
  //
  // It reads which track is playing and not only the position, because with
  // gapless and crossfade the player moves to the next track by itself — the
  // change is announced here rather than decided here. 250 ms rather than the
  // old 500: that interval is also how long the title can lag behind the
  // audio at a transition.
  useEffect(() => {
    if (subscribePlayback) return;
    const t = setInterval(async () => {
      if (currentId == null || paused) return;
      try {
        const st = await playbackState();
        // The player advanced on its own (gapless / crossfade boundary).
        if (st.trackId != null && st.trackId !== currentId) {
          setCurrentId(st.trackId);
          setPosMs(st.posMs);
          return;
        }
        // The player stopped at the end of a track.
        //
        // With autoplay on and somewhere left to go, that is not where we
        // should end up: the seamless hand-off did not happen for whatever
        // reason, so the queue is advanced from here instead. It costs a gap
        // where there should not have been one, which is worth far more than
        // playback quietly stopping mid-list.
        if (!st.playing) {
          if (prefs.autoplay && followerOfRef.current(currentId) != null) {
            onTrackEndedRef.current();
            return;
          }
          // Nothing follows: end of the queue, or autoplay off. Stays on the
          // track, paused — the same place the skip button leaves you.
          setPosMs(st.posMs);
          setPaused(true);
          return;
        }
        if (Date.now() >= seekGuard.current) setPosMs(st.posMs);
      } catch {}
    }, 250);
    return () => clearInterval(t);
  }, [currentId, paused, prefs.autoplay]);

  // Android: position, track end and play/pause arrive pushed by the plugin
  // instead of by polling.
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

  // Notification buttons (Android). Sent by MainActivity.kt, which listens
  // to the plugin's notification broadcasts — see the long comment there.
  useEffect(() => {
    const w = window as typeof window & {
      __swayMediaButton?: (button: string) => void;
      __swayAppVisible?: (visible: boolean) => void;
    };
    // MainActivity listens over two different paths (MediaSession and the
    // notification broadcast) because which one is active depends on the
    // Android version and the manufacturer's layer. If the same tap arrives
    // through both, this keeps the first one.
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

  // Network and battery: on Android only the webview can read them, and
  // auto-sync needs them so it doesn't burn data or the last battery bar
  // without anyone asking for it. Does nothing on desktop.
  useEffect(() => watchConditions(), []);

  // Import progress (copying to the managed folder).
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

  // The library changed from outside this screen: new files the watcher
  // found, or changes brought in by sync from another device.
  //
  // The open playlist has to be reloaded TOO. Without that, the other device
  // moves or removes a song, the local DB is already fine, and the view keeps
  // showing the old order until you click the playlist again.
  // How many tracks there were last time, so as not to announce what didn't change.
  const libCount = useRef(0);
  useEffect(() => {
    libCount.current = library.length;
  }, [library]);

  useEffect(() => {
    if (!('__TAURI_INTERNALS__' in window)) return;
    const un = listen('library-changed', async () => {
      const before = libCount.current;
      const tracks = await refreshLibrary();
      refreshPlaylists();
      refreshPlaylistTracks();
      // The banner is for what shows up on its own (the watcher found new
      // files). A sync already reports its own summary, so announcing
      // "Library updated" on top of it just covers it up without adding anything.
      if (tracks.length !== before) setStatus('Library updated');
    });
    return () => {
      un.then((f) => f());
    };
  }, [refreshLibrary, refreshPlaylists, refreshPlaylistTracks]);

  // The status auto-hides (a brief toast, not persistent text).
  useEffect(() => {
    if (!status) return;
    const t = setTimeout(() => setStatus(''), 2600);
    return () => clearTimeout(t);
  }, [status]);

  // Spacebar = play/pause (unless typing or with a modal open).
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

  // Drawer gesture (only where the sidebar IS a drawer, same breakpoint as
  // the CSS): dragging right from anywhere on the screen opens it, dragging
  // left while open closes it. Never calls preventDefault, so it doesn't
  // steal the table's scroll.
  useEffect(() => {
    const TRIGGER_PX = 70; // minimum horizontal travel
    const SLOP_PX = 45; // vertical travel that cancels (it's a scroll)
    // Controls that are handled by dragging horizontally: there the gesture
    // belongs to the control, not the drawer.
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
      // With very little travel the direction is pure noise: only once past
      // the dead zone does it make sense to decide whether it's a horizontal
      // gesture or a scroll (otherwise any initial jitter cancels the gesture).
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
  // With an active search, it ALWAYS searches the whole library, regardless
  // of the selected playlist.
  const baseTracks = searching ? library : selection.type === 'library' ? library : plTracks;
  // Looking inside a playlist this device doesn't sync. Doesn't count with an
  // active search: that searches the whole library.
  const inUnselectedPlaylist =
    !searching &&
    selection.type === 'playlist' &&
    nodes.find((n) => n.id === selection.id)?.inScope === false;

  const visibleTracks = useMemo(() => {
    // Out of scope and with no file here: the space has already been freed,
    // it has no business in the main view. While the file still takes up
    // space it keeps showing, dimmed, so it's clear it's on its way out.
    //
    // Inside an unchecked playlist, everything that still has a file here
    // shows, dimmed, including what's also in a checked one: if a track still
    // takes up space, hiding it from a list where it appears would be lying.
    // What decides whether the playlist itself still exists is a separate
    // count (`strandedCount`), and there the borrowed track doesn't count.
    const shown = baseTracks.filter((t) =>
      inUnselectedPlaylist ? t.present : t.inScope || t.present,
    );
    const q = search.trim().toLowerCase();
    if (!q) return shown;
    return shown.filter((t) =>
      [t.title, t.artist, t.album, t.genre].some((f) => f.toLowerCase().includes(q)),
    );
  }, [baseTracks, search, inUnselectedPlaylist]);

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

  // Keeps the player mounted during its exit animation (on stop).
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

  /// Which track follows `id`, by the same rules the transport buttons use.
  /// `null` = nothing follows, which is also how autoplay-off is expressed to
  /// the player: it is simply never told what comes next. Repeat means the
  /// next track is this one again.
  const followerOf = useCallback(
    (id: number | null): number | null => {
      if (id == null || !prefs.autoplay) return null;
      if (repeat === 'track' || repeat === 'once') return id;
      const q = queueRef.current;
      return q[q.indexOf(id) + 1] ?? null;
    },
    [prefs.autoplay, repeat],
  );
  // The state poll is declared above this point and needs it; a ref keeps the
  // two independent of declaration order.
  const followerOfRef = useRef(followerOf);
  followerOfRef.current = followerOf;

  /// Starts a track and immediately tells the player what follows it.
  ///
  /// The two have to travel together. Starting a track clears whatever the
  /// player had queued behind the old one — it belongs to a queue position
  /// that no longer applies — so anything that plays a track owes it a new
  /// follower. Leaving that to an effect keyed on the current track silently
  /// fails when the SAME track is played again: the id doesn't change, the
  /// effect doesn't re-run, and the player sits with nothing queued and stops
  /// dead at the end of it.
  const startTrack = useCallback(
    async (id: number) => {
      await playTrack(id);
      setNextTrack(followerOf(id)).catch(() => {});
    },
    [followerOf],
  );

  const onPlay = useCallback(
    async (id: number) => {
      // A track out of scope shows and can be organized, but won't play,
      // whether the file is there or not: it's outside what this device
      // syncs, and letting it play until someone frees space would make the
      // same track work today and not tomorrow. Saying so is better than an
      // error with no context.
      const t = visibleTracks.find((x) => x.id === id);
      if (t && !t.inScope) {
        setStatus('Out of sync scope — select its playlist in Sync to use it here');
        return;
      }
      if (t?.present === false) {
        setStatus('Not on this device — select its playlist in Sync to download it');
        return;
      }
      try {
        queueRef.current = buildQueue(visibleTracks.map((t) => t.id), id, shuffle);
        await startTrack(id);
        setCurrentId(id);
        setPaused(false);
        setPosMs(0);
      } catch (e) {
        setStatus('Playback error: ' + e);
      }
    },
    [visibleTracks, shuffle, startTrack],
  );

  const playOffset = useCallback(
    async (delta: number) => {
      const q = queueRef.current;
      if (currentId == null || q.length === 0) return;
      const next = q[q.indexOf(currentId) + delta];
      if (next == null) {
        // Edge of the queue. The current track is never released: if
        // `currentId` were cleared, the app would end up with nothing
        // selected while the native player still has the audio loaded — and
        // from that state `playOffset` exits through the early return above,
        // meaning you could no longer go forward or back without picking a
        // track by hand again.
        if (delta < 0) onSeek(0); // was already the first one: starts over
        else {
          await pausePlayback(); // end of the queue: stops, but stays there
          setPaused(true);
        }
        return;
      }
      await startTrack(next);
      setCurrentId(next);
      setPaused(false);
      setPosMs(0);
    },
    [currentId, startTrack],
  );
  const playOffsetRef = useRef(playOffset);
  playOffsetRef.current = playOffset;
  const posMsRef = useRef(posMs);
  posMsRef.current = posMs;

  // Keeps the follower correct when the queue or the rules change underneath
  // a track that is already playing (shuffle toggled, repeat cycled, autoplay
  // switched off). Playing a track is handled by `startTrack`, not here.
  useEffect(() => {
    setNextTrack(followerOf(currentId)).catch(() => {});
  }, [followerOf, currentId, shuffle]);


  /// Desktop only, and only while a seamless mode is on: there the player
  /// advances by itself and the poll reports it. Everywhere else the app still
  /// deduces the end of the track from the position.
  const playerDrivesTransitions =
    !subscribePlayback && (prefs.gapless || prefs.crossfadeSecs > 0);

  // Track end: repeat mode governs the current track, not the queue. 'once'
  // turns itself off after repeating, so the next lap moves on normally.
  //
  // The `advancing` lock exists because track end can arrive more than once
  // before the position resets (on desktop it's deduced from the position,
  // which stays past the end for a few more renders). Without it, 'once'
  // turns off repeat, the effect re-enters with the old position and skips a
  // track.
  const advancing = useRef(false);
  const onTrackEnded = useCallback(async () => {
    if (currentId == null || advancing.current) return;
    // Autoplay off: the track ends and playback stays there, on it. Same
    // place the end of the queue leaves you.
    if (!prefs.autoplay) {
      await pausePlayback();
      setPaused(true);
      return;
    }
    advancing.current = true;
    try {
      if (repeat === 'track' || repeat === 'once') {
        if (repeat === 'once') {
          setRepeat('off');
          localStorage.setItem('sway.repeat', 'off');
        }
        await startTrack(currentId);
        setPaused(false);
        setPosMs(0);
      } else {
        await playOffset(1);
      }
    } finally {
      advancing.current = false;
    }
  }, [currentId, repeat, playOffset, prefs.autoplay, startTrack]);
  const onTrackEndedRef = useRef(onTrackEnded);
  onTrackEndedRef.current = onTrackEnded;

  // Auto-advance on desktop: there's no track-end event, it's deduced from
  // the position. On Android it's triggered by the plugin (see the
  // subscription effect).
  //
  // Skipped entirely while a seamless mode is on: there the player already
  // moved to the next track before this position was ever reported, so acting
  // on it here would skip a second one.
  useEffect(() => {
    if (subscribePlayback || playerDrivesTransitions) return;
    if (current && !paused && current.durationMs > 0 && posMs >= current.durationMs - 600) {
      onTrackEnded();
    }
  }, [posMs, current, paused, onTrackEnded, playerDrivesTransitions]);

  function onToggleShuffle() {
    setShuffle((s) => {
      const next = !s;
      localStorage.setItem('sway.shuffle', next ? '1' : '0');
      // Rebuilds the queue keeping the current track first.
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
      // On mobile there's no tooltip: the toast is the only way to know what
      // state the button ended up in.
      setStatus(REPEAT_LABEL[next]);
      return next;
    });
  }

  async function onToggle() {
    if (!paused) {
      await pausePlayback();
      setPaused(true);
      return;
    }
    // Play on a track that already finished: there is nothing left to resume
    // — the source ran out — so it starts over. Without this the button
    // flips back to paused a moment later and looks broken, which is what
    // happens whenever the queue ends or autoplay is off.
    if (current && current.durationMs > 0 && posMs >= current.durationMs - 1000) {
      await startTrack(current.id);
      setPosMs(0);
      setPaused(false);
      return;
    }
    await resumePlayback();
    setPaused(false);
  }

  async function onStop() {
    await stopPlayback();
    setCurrentId(null);
    setPaused(false);
    setPosMs(0);
  }

  // Standard "back" from any player: returns to the start of the track, and
  // only moves to the previous one if you were already at the start. Shared
  // by the player's button and the notification's.
  function onPrev() {
    if (posMsRef.current > RESTART_MS) onSeek(0);
    else playOffsetRef.current(-1);
  }

  async function onSeek(secs: number) {
    seekGuard.current = Date.now() + 800;
    setPosMs(secs * 1000);
    await seekTo(secs);
  }

  // Gain: the per-track trim, saved on the track. Separate from the volume
  // above on purpose — volume is the room, gain is this record. Turning up a
  // quiet track has to survive it coming round again, which is why it goes to
  // the DB and not to a slider that resets.
  //
  // Written through with the same throttle as the volume (dragging a knob
  // fires per pixel), and mirrored into the loaded lists so the value shown
  // doesn't wait for a library refresh.
  const gainPending = useRef<{ id: number; db: number } | null>(null);
  function onGain(gainDb: number) {
    if (currentId == null) return;
    const id = currentId;
    const patch = (ts: Track[]) =>
      ts.map((t) => (t.id === id ? { ...t, gainDb } : t));
    setLibrary(patch);
    setPlTracks(patch);
    const first = gainPending.current == null;
    gainPending.current = { id, db: gainDb };
    if (first) {
      setTimeout(() => {
        const p = gainPending.current;
        gainPending.current = null;
        if (p) setTrackGain(p.id, p.db).catch(() => {});
      }, 60);
    }
  }

  // Playback preferences. Saved in the backend (the audio thread reads them
  // too), so the screen sends the whole object and keeps the copy it just
  // sent — no reload round trip to see the switch move.
  const onPrefs = useCallback(async (next: PlaybackPrefs) => {
    setPrefs(next);
    try {
      await setPlaybackPrefs(next);
    } catch (e) {
      setStatus('Could not save playback settings: ' + e);
    }
  }, []);

  // Volume: the UI responds instantly, the backend updates with throttling.
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

  // --- OS file drop ---------------------------------------------------------

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
        const base = paths[0].replace(/[\\/]+$/, '').split(/[\\/]/).pop() ?? 'Imported';
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
      // Keeps the 100% visible for a moment before hiding the toast.
      setTimeout(() => setImportProgress(null), 1000);
    }
  };

  useEffect(() => {
    // Only inside Tauri (a regular browser has no OS file drop).
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

  // --- Internal drag (tracks and nodes) --------------------------------------

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
    // node payload
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

  // --- Organization -----------------------------------------------------------

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

  // Referentially stable wrappers for the table: their identity doesn't
  // change between renders, so React.memo avoids re-rendering the ~1149 rows
  // when only the player state changes (position, pause, volume).
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

  /// What shows in the main view's tree. An unchecked playlist stays there
  /// while it's taking up space —dimmed, so it's clear it's on its way out—
  /// and disappears entirely once its space is freed.
  ///
  /// A folder stays if anything hanging off it stays, even if the folder
  /// itself is out of scope: checking just one playlist inside is normal, and
  /// without this rule that playlist would be visible but unreachable.
  ///
  /// The scope editor does NOT use this: there, everything shows, or there'd
  /// be no way to re-check what got hidden.
  const visibleNodes = useMemo(() => {
    const kids = new Map<number | null, PlaylistNode[]>();
    for (const n of nodes) {
      const list = kids.get(n.parentId);
      list ? list.push(n) : kids.set(n.parentId, [n]);
    }
    const keep = new Set<number>();
    const walk = (n: PlaylistNode): boolean => {
      // Children first, without short-circuiting on the folder's own result:
      // the folder stays if anything inside stays.
      let anyKid = false;
      for (const k of kids.get(n.id) ?? []) if (walk(k)) anyKid = true;
      const stays = n.inScope || n.strandedCount > 0 || anyKid;
      if (stays) keep.add(n.id);
      return stays;
    };
    for (const root of kids.get(null) ?? []) walk(root);
    return nodes.filter((n) => keep.has(n.id));
  }, [nodes]);

  // The open playlist may have gotten hidden (its space was freed) while you
  // were looking at it: staying on a view that's no longer in the tree leaves
  // the app with no way back.
  useEffect(() => {
    if (selection.type !== 'playlist') return;
    if (nodes.length > 0 && !visibleNodes.some((n) => n.id === selection.id)) {
      setSelection({ type: 'library' });
    }
  }, [visibleNodes, nodes.length, selection]);

  const selectedNode =
    selection.type === 'playlist' ? nodes.find((n) => n.id === selection.id) : null;

  const playlistOptions = useMemo(() => {
    const path = (n: PlaylistNode): string => {
      const parent = visibleNodes.find((x) => x.id === n.parentId);
      return parent ? path(parent) + ' / ' + n.name : n.name;
    };
    // Only the ones in scope: sending tracks to a playlist this device
    // doesn't sync is asking for them to leave the moment they're saved.
    return visibleNodes
      .filter((n) => n.kind === 'playlist' && n.inScope)
      .map((n) => ({ id: n.id, label: path(n) }));
  }, [visibleNodes]);

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
          nodes={visibleNodes}
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
          onGain={onGain}
          onToggleShuffle={onToggleShuffle}
          onCycleRepeat={onCycleRepeat}
          showVolume={!isAndroid()}
          showGain={!isAndroid()}
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
          prefs={prefs}
          onPrefs={onPrefs}
          onClose={() => setModal(null)}
          onStatus={setStatus}
          onImported={async () => {
            await Promise.all([refreshLibrary(), refreshPlaylists()]);
          }}
          onOpenSync={() => setModal({ type: 'sync' })}
        />
      )}
      {modal?.type === 'sync' && (
        <Sync
          nodes={nodes}
          onClose={() => setModal(null)}
          onStatus={setStatus}
          onLibraryChanged={async () => {
            await Promise.all([refreshLibrary(), refreshPlaylists(), refreshPlaylistTracks()]);
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
