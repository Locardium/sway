import { useState } from 'react';
import { Track } from '../api';

interface Props {
  tracks: Track[];
  currentId: number | null;
  selected: Set<number>;
  onSelectedChange: (sel: Set<number>) => void;
  onPlay: (id: number) => void;
  /** Reorden manual habilitado (vista playlist, sin búsqueda activa). */
  canReorder: boolean;
  onReorder: (trackIds: number[], index: number) => void;
  /** Quitar de la playlist (null en biblioteca). */
  onRemove: ((trackIds: number[]) => void) | null;
}

const TRACKS_MIME = 'application/x-sway-tracks';

function fmt(ms: number): string {
  if (!ms || isNaN(ms)) return '0:00';
  const s = Math.floor(ms / 1000);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

export default function TrackTable({
  tracks,
  currentId,
  selected,
  onSelectedChange,
  onPlay,
  canReorder,
  onReorder,
  onRemove,
}: Props) {
  const [anchor, setAnchor] = useState<number | null>(null);
  const [dropIdx, setDropIdx] = useState<number | null>(null);

  function onRowClick(e: React.MouseEvent, t: Track, idx: number) {
    if (e.shiftKey && anchor != null) {
      const a = tracks.findIndex((x) => x.id === anchor);
      if (a >= 0) {
        const [lo, hi] = a < idx ? [a, idx] : [idx, a];
        onSelectedChange(new Set(tracks.slice(lo, hi + 1).map((x) => x.id)));
        return;
      }
    }
    if (e.ctrlKey || e.metaKey) {
      const next = new Set(selected);
      next.has(t.id) ? next.delete(t.id) : next.add(t.id);
      onSelectedChange(next);
    } else {
      onSelectedChange(new Set([t.id]));
    }
    setAnchor(t.id);
  }

  function dragIds(t: Track): number[] {
    return selected.has(t.id) ? [...selected] : [t.id];
  }

  function rowDropIndex(e: React.DragEvent, idx: number): number {
    const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
    return e.clientY - r.top < r.height / 2 ? idx : idx + 1;
  }

  function onKeyDown(e: React.KeyboardEvent) {
    if (e.key === 'Delete' && onRemove && selected.size > 0) {
      onRemove([...selected]);
      onSelectedChange(new Set());
    }
    if ((e.ctrlKey || e.metaKey) && e.key === 'a') {
      e.preventDefault();
      onSelectedChange(new Set(tracks.map((t) => t.id)));
    }
  }

  return (
    <div className="table-wrap" tabIndex={0} onKeyDown={onKeyDown}>
      <table className="tracks">
        <thead>
          <tr>
            <th className="col-play"></th>
            <th>Título</th>
            <th>Artista</th>
            <th>Álbum</th>
            <th>Género</th>
            <th className="num">BPM</th>
            <th className="num">Dur.</th>
          </tr>
        </thead>
        <tbody
          onDragLeave={(e) => {
            if (e.target === e.currentTarget) setDropIdx(null);
          }}
        >
          {tracks.map((t, idx) => (
            <tr
              key={t.id}
              className={[
                t.id === currentId ? 'playing' : '',
                selected.has(t.id) ? 'selected' : '',
                dropIdx === idx ? 'drop-before' : '',
                dropIdx === idx + 1 && idx === tracks.length - 1 ? 'drop-after' : '',
              ].join(' ')}
              draggable
              onDragStart={(e) => {
                e.dataTransfer.setData(TRACKS_MIME, JSON.stringify(dragIds(t)));
                e.dataTransfer.effectAllowed = 'copyMove';
              }}
              onDragOver={(e) => {
                if (canReorder && e.dataTransfer.types.includes(TRACKS_MIME)) {
                  e.preventDefault();
                  setDropIdx(rowDropIndex(e, idx));
                }
              }}
              onDrop={(e) => {
                if (!canReorder) return;
                e.preventDefault();
                const data = e.dataTransfer.getData(TRACKS_MIME);
                setDropIdx(null);
                if (data) onReorder(JSON.parse(data), rowDropIndex(e, idx));
              }}
              onClick={(e) => onRowClick(e, t, idx)}
              onDoubleClick={() => onPlay(t.id)}
            >
              <td className="col-play">
                <button className="mini" onClick={(e) => { e.stopPropagation(); onPlay(t.id); }}>
                  {t.id === currentId ? '♫' : '▶'}
                </button>
              </td>
              <td className="t-title">{t.title}</td>
              <td>{t.artist}</td>
              <td>{t.album}</td>
              <td>{t.genre}</td>
              <td className="num">{t.bpm ?? ''}</td>
              <td className="num">{fmt(t.durationMs)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
