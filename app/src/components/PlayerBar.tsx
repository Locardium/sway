import { useRef } from 'react';
import {
  Info,
  Pause,
  Play,
  Repeat,
  Repeat1,
  Shuffle,
  SkipBack,
  SkipForward,
  Square,
  Volume2,
  VolumeX,
} from 'lucide-react';
import { Track } from '../api';
import Cover from './Cover';

export type RepeatMode = 'off' | 'all' | 'one';

interface Props {
  track: Track;
  paused: boolean;
  posMs: number;
  volume: number;
  shuffle: boolean;
  repeat: RepeatMode;
  infoOpen: boolean;
  onToggle: () => void;
  onStop: () => void;
  onPrev: () => void;
  onNext: () => void;
  onSeek: (secs: number) => void;
  onVolume: (v: number) => void;
  onToggleShuffle: () => void;
  onCycleRepeat: () => void;
  onToggleInfo: () => void;
}

function fmt(ms: number): string {
  if (!ms || isNaN(ms)) return '0:00';
  const s = Math.floor(ms / 1000);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

export default function PlayerBar({
  track,
  paused,
  posMs,
  volume,
  shuffle,
  repeat,
  infoOpen,
  onToggle,
  onStop,
  onPrev,
  onNext,
  onSeek,
  onVolume,
  onToggleShuffle,
  onCycleRepeat,
  onToggleInfo,
}: Props) {
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
        <Cover trackId={track.id} className="np-art" eager />
        <div className="np-text">
          <strong>{track.title}</strong>
          <span className="np-artist">{track.artist}</span>
        </div>
      </div>
      <div className="player-center">
        <div className="controls">
          <button
            className={'ctl side' + (shuffle ? ' on' : '')}
            onClick={onToggleShuffle}
            title={shuffle ? 'Shuffle activado' : 'Shuffle'}
          >
            <Shuffle size={15} />
          </button>
          <button className="ctl" onClick={onPrev} title="Anterior">
            <SkipBack size={17} fill="currentColor" />
          </button>
          <button className="ctl main" onClick={onToggle} title={paused ? 'Reproducir' : 'Pausa'}>
            {paused ? <Play size={17} fill="currentColor" /> : <Pause size={17} fill="currentColor" />}
          </button>
          <button className="ctl" onClick={onNext} title="Siguiente">
            <SkipForward size={17} fill="currentColor" />
          </button>
          <button
            className={'ctl side' + (repeat !== 'off' ? ' on' : '')}
            onClick={onCycleRepeat}
            title={repeat === 'off' ? 'Repetir' : repeat === 'all' ? 'Repetir todo' : 'Repetir uno'}
          >
            {repeat === 'one' ? <Repeat1 size={15} /> : <Repeat size={15} />}
          </button>
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
        <button className="ctl stop" onClick={onStop} title="Detener">
          <Square size={13} fill="currentColor" />
        </button>
        <div className="vol" title={`Volumen ${Math.round(volume * 100)}%`}>
          <button
            className="ctl side"
            onClick={() => onVolume(volume === 0 ? 1 : 0)}
            title={volume === 0 ? 'Activar sonido' : 'Silenciar'}
          >
            {volume === 0 ? <VolumeX size={15} /> : <Volume2 size={15} />}
          </button>
          <input
            type="range"
            min={0}
            max={100}
            value={Math.round(volume * 100)}
            onChange={(e) => onVolume(Number(e.target.value) / 100)}
            style={{ ['--vol' as string]: `${Math.round(volume * 100)}%` }}
            aria-label="Volumen"
          />
        </div>
        <button
          className={'ctl side' + (infoOpen ? ' on' : '')}
          onClick={onToggleInfo}
          title="Info del track"
        >
          <Info size={15} />
        </button>
      </div>
    </footer>
  );
}
