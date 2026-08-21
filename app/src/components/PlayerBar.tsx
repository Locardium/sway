import { useRef, useState } from 'react';
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
import Knob from './Knob';

/// `track` = repeats the current one until you turn it off. `once` = repeats
/// it one more time and turns itself off, then continues with the queue.
export type RepeatMode = 'off' | 'track' | 'once';

export const REPEAT_LABEL: Record<RepeatMode, string> = {
  off: 'Repeat off',
  track: 'Repeat this track',
  once: 'Repeat once, then continue',
};

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
  onGain: (gainDb: number) => void;
  onToggleShuffle: () => void;
  onCycleRepeat: () => void;
}

/// Range of the gain knob, matching `db::MAX_GAIN_DB` in the backend (which
/// clamps to the same figure). ±12 dB is the usual travel on a mixer's trim.
const GAIN_RANGE = 12;

function fmtGain(db: number): string {
  return `${db > 0 ? '+' : ''}${db.toFixed(1)} dB`;
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
  onGain,
  onToggleShuffle,
  onCycleRepeat,
}: Props) {
  const barRef = useRef<HTMLDivElement>(null);
  // While dragging, the bar shows the finger/mouse position and the actual
  // seek is only sent on release (a seek per pixel moved overworks the
  // backend and looks worse).
  const [dragMs, setDragMs] = useState<number | null>(null);
  const shownMs = dragMs ?? posMs;
  const pct = track.durationMs > 0 ? Math.min(100, (shownMs / track.durationMs) * 100) : 0;

  function msFromClientX(clientX: number) {
    const r = barRef.current!.getBoundingClientRect();
    const frac = Math.min(1, Math.max(0, (clientX - r.left) / r.width));
    return frac * track.durationMs;
  }

  function onBarPointerDown(e: React.PointerEvent) {
    if (track.durationMs <= 0) return;
    e.currentTarget.setPointerCapture(e.pointerId);
    setDragMs(msFromClientX(e.clientX));
  }

  function onBarPointerMove(e: React.PointerEvent) {
    if (dragMs == null) return;
    setDragMs(msFromClientX(e.clientX));
  }

  function onBarPointerUp(e: React.PointerEvent) {
    if (dragMs == null) return;
    const ms = msFromClientX(e.clientX);
    setDragMs(null);
    onSeek(Math.floor(ms / 1000));
  }

  return (
    <footer className={'player' + (closing ? ' closing' : '')}>
      <div className="np">
        {/* Stop lives on the artwork, revealed on hover. It's the one
            transport action you almost never want mid-set, and it was taking
            a permanent slot next to controls used constantly. Kept reachable
            (and always visible on touch, where there is no hover — see the CSS). */}
        <div className="np-art-wrap">
          <Cover trackId={track.id} className="np-art" eager />
          <button className="art-stop" onClick={onStop} title="Stop" aria-label="Stop">
            <Square size={13} fill="currentColor" />
          </button>
        </div>
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
            title={REPEAT_LABEL[repeat]}
            aria-label={REPEAT_LABEL[repeat]}
          >
            {repeat === 'once' ? <Repeat1 size={15} /> : <Repeat size={15} />}
          </button>
        </div>
        <div className="seek">
          <span className="time">{fmt(shownMs)}</span>
          <div
            className={'seek-bar' + (dragMs != null ? ' dragging' : '')}
            ref={barRef}
            onPointerDown={onBarPointerDown}
            onPointerMove={onBarPointerMove}
            onPointerUp={onBarPointerUp}
            onPointerCancel={() => setDragMs(null)}
          >
            <div className="seek-fill" style={{ width: `${pct}%` }} />
            <div className="seek-knob" style={{ left: `${pct}%` }} />
          </div>
          <span className="time">{fmt(track.durationMs)}</span>
        </div>
      </div>
      <div className="player-right">
        <div className="gain">
          <Knob
            value={track.gainDb}
            min={-GAIN_RANGE}
            max={GAIN_RANGE}
            center={0}
            step={0.5}
            size={26}
            label="Gain"
            format={fmtGain}
            onChange={onGain}
          />
          <span className="gain-value">{fmtGain(track.gainDb)}</span>
        </div>
        {/* Wheel over the whole volume group, not just the slider: the
            pointer is usually on its way past the icon, and having to land
            on a 4 px track first would defeat the point. */}
        <div
          className="vol"
          onWheel={(e) => {
            const step = e.shiftKey ? 0.01 : 0.05;
            const next = volume + (e.deltaY < 0 ? step : -step);
            onVolume(Math.min(1, Math.max(0, Math.round(next * 100) / 100)));
          }}
        >
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
