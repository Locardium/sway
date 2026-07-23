import { useRef } from 'react';
import { Track } from '../api';

interface Props {
  track: Track;
  paused: boolean;
  posMs: number;
  onToggle: () => void;
  onStop: () => void;
  onPrev: () => void;
  onNext: () => void;
  onSeek: (secs: number) => void;
}

function fmt(ms: number): string {
  if (!ms || isNaN(ms)) return '0:00';
  const s = Math.floor(ms / 1000);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

export default function PlayerBar({ track, paused, posMs, onToggle, onStop, onPrev, onNext, onSeek }: Props) {
  const barRef = useRef<HTMLDivElement>(null);
  const pct = track.durationMs > 0 ? Math.min(100, (posMs / track.durationMs) * 100) : 0;

  function seekFromEvent(e: React.MouseEvent) {
    const r = barRef.current!.getBoundingClientRect();
    const frac = Math.min(1, Math.max(0, (e.clientX - r.left) / r.width));
    onSeek(Math.floor((frac * track.durationMs) / 1000));
  }

  return (
    <footer className="player">
      <div className="np">
        <strong>{track.title}</strong>
        <span className="np-artist">{track.artist}</span>
      </div>
      <div className="player-center">
        <div className="controls">
          <button className="ctl" onClick={onPrev} title="Anterior">⏮</button>
          <button className="ctl main" onClick={onToggle} title={paused ? 'Reproducir' : 'Pausa'}>
            {paused ? '▶' : '⏸'}
          </button>
          <button className="ctl" onClick={onNext} title="Siguiente">⏭</button>
          <button className="ctl" onClick={onStop} title="Detener">⏹</button>
        </div>
        <div className="seek">
          <span className="time">{fmt(posMs)}</span>
          <div className="seek-bar" ref={barRef} onClick={seekFromEvent}>
            <div className="seek-fill" style={{ width: `${pct}%` }} />
            <div className="seek-knob" style={{ left: `${pct}%` }} />
          </div>
          <span className="time">{fmt(track.durationMs)}</span>
        </div>
      </div>
      <div className="player-right">
        {track.bpm != null && <span className="bpm-chip">{track.bpm} BPM</span>}
      </div>
    </footer>
  );
}
