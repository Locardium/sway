import { Track } from '../api';

interface Props {
  open: boolean;
  track: Track | null;
  onClose: () => void;
}

function fmt(ms: number): string {
  if (!ms || isNaN(ms)) return '0:00';
  const s = Math.floor(ms / 1000);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

export default function RightPanel({ open, track, onClose }: Props) {
  return (
    <aside className={'right-panel' + (open ? ' open' : '')} aria-hidden={!open}>
      <div className="rp-inner">
        <div className="rp-head">
          <span>Ahora suena</span>
          <button className="mini" onClick={onClose} aria-label="Cerrar panel">✕</button>
        </div>
        {track ? (
          <>
            <div className="rp-art" aria-hidden="true">♪</div>
            <h3 className="rp-title">{track.title}</h3>
            <p className="rp-artist">{track.artist || '—'}</p>
            <dl className="rp-meta">
              <dt>Álbum</dt>
              <dd>{track.album || '—'}</dd>
              <dt>Género</dt>
              <dd>{track.genre || '—'}</dd>
              <dt>BPM</dt>
              <dd>{track.bpm ?? '—'}</dd>
              <dt>Duración</dt>
              <dd>{fmt(track.durationMs)}</dd>
              <dt>Archivo</dt>
              <dd className="rp-path" title={track.path}>{track.path}</dd>
            </dl>
          </>
        ) : (
          <p className="rp-empty">Nada sonando. Doble click en un track para reproducir.</p>
        )}
      </div>
    </aside>
  );
}
