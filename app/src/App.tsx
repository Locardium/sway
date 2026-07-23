import { useEffect, useState, useCallback } from 'react';
import { open } from '@tauri-apps/plugin-dialog';
import {
  Track,
  importFolder,
  listTracks,
  playTrack,
  pausePlayback,
  resumePlayback,
  stopPlayback,
  playbackPosition,
} from './api';

function fmt(ms: number): string {
  if (!ms || isNaN(ms)) return '0:00';
  const s = Math.floor(ms / 1000);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

export default function App() {
  const [tracks, setTracks] = useState<Track[]>([]);
  const [currentId, setCurrentId] = useState<number | null>(null);
  const [paused, setPaused] = useState(false);
  const [posMs, setPosMs] = useState(0);
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState('');

  const refresh = useCallback(async () => {
    try {
      setTracks(await listTracks());
    } catch (e) {
      setStatus('No pude leer tracks: ' + e);
    }
  }, []);

  // Reintenta un par de veces por si el backend aun no registro el estado.
  useEffect(() => {
    let tries = 0;
    let done = false;
    const attempt = async () => {
      try {
        const t = await listTracks();
        setTracks(t);
        done = true;
      } catch {
        if (tries++ < 5 && !done) setTimeout(attempt, 300);
      }
    };
    attempt();
  }, []);

  // Poll de posicion de reproduccion.
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

  async function onImport() {
    const folder = await open({ directory: true, multiple: false, title: 'Elegí tu carpeta de música' });
    if (!folder || typeof folder !== 'string') return;
    setBusy(true);
    setStatus('Importando…');
    try {
      const n = await importFolder(folder);
      setStatus(`Importados ${n} tracks.`);
      await refresh();
    } catch (e) {
      setStatus('Error import: ' + e);
    } finally {
      setBusy(false);
    }
  }

  async function onPlay(id: number) {
    await playTrack(id);
    setCurrentId(id);
    setPaused(false);
    setPosMs(0);
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

  const current = tracks.find((t) => t.id === currentId) || null;

  return (
    <div className="app">
      <header>
        <h1>Sway</h1>
        <button onClick={onImport} disabled={busy}>
          {busy ? '…' : '+ Importar carpeta'}
        </button>
        <span className="status">{status || `${tracks.length} tracks`}</span>
      </header>

      <main>
        <table className="tracks">
          <thead>
            <tr>
              <th></th>
              <th>Título</th>
              <th>Artista</th>
              <th>Álbum</th>
              <th>Género</th>
              <th>BPM</th>
              <th>Dur.</th>
            </tr>
          </thead>
          <tbody>
            {tracks.map((t) => (
              <tr
                key={t.id}
                className={t.id === currentId ? 'active' : ''}
                onDoubleClick={() => onPlay(t.id)}
              >
                <td>
                  <button className="mini" onClick={() => onPlay(t.id)}>
                    ▶
                  </button>
                </td>
                <td>{t.title}</td>
                <td>{t.artist}</td>
                <td>{t.album}</td>
                <td>{t.genre}</td>
                <td>{t.bpm ?? ''}</td>
                <td>{fmt(t.durationMs)}</td>
              </tr>
            ))}
          </tbody>
        </table>
        {tracks.length === 0 && <p className="empty">Importá una carpeta para empezar.</p>}
      </main>

      {current && (
        <footer className="player">
          <div className="np">
            <strong>{current.title}</strong> — {current.artist}
          </div>
          <div className="controls">
            <button onClick={onToggle}>{paused ? '▶' : '⏸'}</button>
            <button onClick={onStop}>⏹</button>
            <span className="pos">
              {fmt(posMs)} / {fmt(current.durationMs)}
            </span>
          </div>
        </footer>
      )}
    </div>
  );
}
