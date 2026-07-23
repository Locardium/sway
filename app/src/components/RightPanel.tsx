import { X } from 'lucide-react';
import { Track } from '../api';
import Cover from './Cover';

interface Props {
  open: boolean;
  track: Track | null;
  isPlaying: boolean;
  onClose: () => void;
}

function fmt(ms: number): string {
  if (!ms || isNaN(ms)) return '0:00';
  const s = Math.floor(ms / 1000);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

export default function RightPanel({ open, track, isPlaying, onClose }: Props) {
  return (
    <aside className={'right-panel' + (open ? ' open' : '')} aria-hidden={!open}>
      <div className="rp-inner">
        <div className="rp-head">
          <span>{isPlaying ? 'Now playing' : 'Details'}</span>
          <button className="mini" onClick={onClose} aria-label="Close panel">
            <X size={14} />
          </button>
        </div>
        {track ? (
          <>
            <Cover trackId={track.id} className="rp-art" eager />
            <h3 className="rp-title">{track.title}</h3>
            <p className="rp-artist">{track.artist || '—'}</p>
            <dl className="rp-meta">
              <dt>Album</dt>
              <dd>{track.album || '—'}</dd>
              <dt>Genre</dt>
              <dd>{track.genre || '—'}</dd>
              <dt>BPM</dt>
              <dd>{track.bpm ?? '—'}</dd>
              <dt>Duration</dt>
              <dd>{fmt(track.durationMs)}</dd>
              <dt>File</dt>
              <dd className="rp-path" title={track.path}>{track.path}</dd>
            </dl>
          </>
        ) : (
          <p className="rp-empty">Click a track to see its details.</p>
        )}
      </div>
    </aside>
  );
}
