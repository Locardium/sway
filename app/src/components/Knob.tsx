import { useCallback, useRef } from 'react';

interface Props {
  value: number;
  min: number;
  max: number;
  /// Where a double-click sends it, and where the indicator reads as
  /// "untouched". Usually the centre of the travel.
  center: number;
  step: number;
  size?: number;
  label: string;
  /// Rendered inside the tooltip and next to the knob.
  format: (v: number) => string;
  onChange: (v: number) => void;
}

/// How far the pointer has to travel, in pixels, to cross the whole range.
/// Deliberately longer than the knob is wide: a mixer knob has a full turn of
/// travel for a range this fine, and matching pixels to degrees would make
/// every small correction a fight.
const TRAVEL_PX = 180;

/// Degrees swept from minimum to maximum, centred on straight up. 270° is the
/// convention on hardware — the 90° gap at the bottom is what makes the
/// pointer's angle readable at a glance.
const SWEEP_DEG = 270;

/// A rotary control, dragged vertically. Double-click returns it to `center`.
///
/// Vertical drag rather than a circular gesture: chasing the pointer around
/// the circumference is precise only near the rim, and this knob is small.
/// Every DAW does the same for the same reason.
export default function Knob({
  value,
  min,
  max,
  center,
  step,
  size = 34,
  label,
  format,
  onChange,
}: Props) {
  // Where the drag started, and the value it started from. Deltas are taken
  // against these rather than accumulated per event, so rounding to `step`
  // can't drift over a long drag.
  const drag = useRef<{ y: number; from: number } | null>(null);

  const clamp = useCallback(
    (v: number) => {
      const snapped = Math.round(v / step) * step;
      return Math.min(max, Math.max(min, snapped));
    },
    [min, max, step],
  );

  function onPointerDown(e: React.PointerEvent) {
    // A double-click arrives as a second pointerdown; let it through to the
    // handler below instead of starting a drag that fights it.
    if (e.detail > 1) return;
    e.currentTarget.setPointerCapture(e.pointerId);
    drag.current = { y: e.clientY, from: value };
  }

  function onPointerMove(e: React.PointerEvent) {
    if (!drag.current) return;
    // Up is louder — the opposite would be a nasty surprise on a gain control.
    const dy = drag.current.y - e.clientY;
    // Fine mode: holding shift stretches the travel, for half-dB nudges.
    const travel = e.shiftKey ? TRAVEL_PX * 4 : TRAVEL_PX;
    onChange(clamp(drag.current.from + (dy / travel) * (max - min)));
  }

  function endDrag(e: React.PointerEvent) {
    if (drag.current && e.currentTarget.hasPointerCapture(e.pointerId)) {
      e.currentTarget.releasePointerCapture(e.pointerId);
    }
    drag.current = null;
  }

  function onWheel(e: React.WheelEvent) {
    onChange(clamp(value + (e.deltaY < 0 ? step : -step)));
  }

  function onKeyDown(e: React.KeyboardEvent) {
    const big = (max - min) / 10;
    const map: Record<string, number> = {
      ArrowUp: step,
      ArrowRight: step,
      ArrowDown: -step,
      ArrowLeft: -step,
      PageUp: big,
      PageDown: -big,
    };
    if (e.key in map) {
      e.preventDefault();
      onChange(clamp(value + map[e.key]));
    } else if (e.key === 'Home') {
      e.preventDefault();
      onChange(center);
    }
  }

  const frac = (value - min) / (max - min);
  const angle = -SWEEP_DEG / 2 + frac * SWEEP_DEG;

  // The arc runs from the centre position to where the knob is now, so an
  // untouched knob draws nothing and any offset reads as a wedge with a
  // direction.
  const r = size / 2 - 3;
  const centerFrac = (center - min) / (max - min);
  const centerAngle = -SWEEP_DEG / 2 + centerFrac * SWEEP_DEG;
  const polar = (deg: number) => {
    const rad = ((deg - 90) * Math.PI) / 180;
    return [size / 2 + r * Math.cos(rad), size / 2 + r * Math.sin(rad)];
  };
  const [ax, ay] = polar(centerAngle);
  const [bx, by] = polar(angle);
  const sweepFlag = angle >= centerAngle ? 1 : 0;
  const atCenter = Math.abs(value - center) < step / 2;

  return (
    <div
      className={'knob' + (atCenter ? '' : ' active')}
      style={{ width: size, height: size }}
      role="slider"
      tabIndex={0}
      aria-label={label}
      aria-valuemin={min}
      aria-valuemax={max}
      aria-valuenow={value}
      aria-valuetext={format(value)}
      title={`${label}: ${format(value)} — drag to adjust, double-click to reset`}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={endDrag}
      onPointerCancel={endDrag}
      onDoubleClick={() => onChange(center)}
      onWheel={onWheel}
      onKeyDown={onKeyDown}
    >
      <svg width={size} height={size} viewBox={`0 0 ${size} ${size}`}>
        {/* Full travel, so the range is visible even at rest. */}
        <path
          className="knob-track"
          d={`M ${polar(-SWEEP_DEG / 2).join(' ')} A ${r} ${r} 0 1 1 ${polar(SWEEP_DEG / 2).join(' ')}`}
          fill="none"
        />
        {!atCenter && (
          <path
            className="knob-arc"
            d={`M ${ax} ${ay} A ${r} ${r} 0 0 ${sweepFlag} ${bx} ${by}`}
            fill="none"
          />
        )}
        <line
          className="knob-pointer"
          x1={size / 2}
          y1={size / 2}
          x2={size / 2}
          y2={4}
          transform={`rotate(${angle} ${size / 2} ${size / 2})`}
        />
      </svg>
    </div>
  );
}
