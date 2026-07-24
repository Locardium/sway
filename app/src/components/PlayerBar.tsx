import { useRef } from 'react';
import {
  Pause,
  Play,
  Repeat,
  Repeat1,
  Shuffle,
  SkipBack,
  SkipForward,
  Square,
  Volume1,
  Volume2,
  VolumeX,
} from 'lucide-react';
import { Track } from '../api';
import Cover from './Cover';

export type RepeatMode = 'off' | 'all' | 'one';

interface Props {
  track: Track;
  closing?: boolean;
  paused: boolean;
  posMs: number;
  volume: number;
  shuffle: boolean;
  repeat: RepeatMode;
  onToggle: () => void;
  onStop: () => void;
  onPrev: () => void;
  onNext: () => void;
  onSeek: (secs: number) => void;
  onVolume: (v: number) => void;
  onToggleShuffle: () => void;
  onCycleRepeat: () => void;
}

function fmt(ms: number): string {
  if (!ms || isNaN(ms)) return '0:00';
  const s = Math.floor(ms / 1000);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

function VolIcon({ v }: { v: number }) {
  if (v === 0) return <VolumeX size={15} />;
  if (v < 0.5) return <Volume1 size={15} />;
  return <Volume2 size={15} />;
}

export default function PlayerBar({
  track,
  closing,
  paused,
  posMs,
  volume,
  shuffle,
  repeat,
  onToggle,
  onStop,
  onPrev,
  onNext,
  onSeek,
  onVolume,
  onToggleShuffle,
  onCycleRepeat,
}: Props) {
  const barRef = useRef<HTMLDivElement>(null);
  const pct = track.durationMs > 0 ? Math.min(100, (posMs / track.durationMs) * 100) : 0;

  function seekFromEvent(e: React.MouseEvent) {
    const r = barRef.current!.getBoundingClientRect();
    const frac = Math.min(1, Math.max(0, (e.clientX - r.left) / r.width));
    onSeek(Math.floor((frac * track.durationMs) / 1000));
  }

  return (
    <footer className={'player' + (closing ? ' closing' : '')}>
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
            title={shuffle ? 'Shuffle on' : 'Shuffle'}
          >
            <Shuffle size={15} />
          </button>
          <button className="ctl" onClick={onPrev} title="Previous">
            <SkipBack size={17} fill="currentColor" />
          </button>
          <button className="ctl main" onClick={onToggle} title={paused ? 'Play' : 'Pause'}>
            {paused ? <Play size={17} fill="currentColor" /> : <Pause size={17} fill="currentColor" />}
          </button>
          <button className="ctl" onClick={onNext} title="Next">
            <SkipForward size={17} fill="currentColor" />
          </button>
          <button
            className={'ctl side' + (repeat !== 'off' ? ' on' : '')}
            onClick={onCycleRepeat}
            title={repeat === 'off' ? 'Repeat' : repeat === 'all' ? 'Repeat all' : 'Repeat one'}
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
        <button className="ctl side stop" onClick={onStop} title="Stop">
          <Square size={13} fill="currentColor" />
        </button>
        <div className="vol">
          <button
            className="ctl side"
            onClick={() => onVolume(volume === 0 ? 1 : 0)}
            title={volume === 0 ? 'Unmute' : 'Mute'}
          >
            <VolIcon v={volume} />
          </button>
          <input
            className="range vol-range"
            type="range"
            min={0}
            max={100}
            value={Math.round(volume * 100)}
            onChange={(e) => onVolume(Number(e.target.value) / 100)}
            style={{ ['--fill' as string]: `${Math.round(volume * 100)}%` }}
            aria-label="Volume"
          />
        </div>
      </div>
    </footer>
  );
}
