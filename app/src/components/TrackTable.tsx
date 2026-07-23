import { memo, useEffect, useRef, useState } from 'react';
import { Play } from 'lucide-react';
import { Track } from '../api';
import ContextMenu, { MenuItem } from './ContextMenu';
import Cover from './Cover';

export type ColKey = 'title' | 'artist' | 'album' | 'genre' | 'bpm' | 'duration';

interface ColDef {
  key: ColKey;
  label: string;
  weight: number; // ancho relativo (proporcional al viewport)
  visible: boolean;
  numeric?: boolean;
}

const DEFAULT_COLS: ColDef[] = [
  { key: 'title', label: 'Title', weight: 3, visible: true },
  { key: 'artist', label: 'Artist', weight: 2, visible: true },
  { key: 'album', label: 'Album', weight: 2, visible: true },
  { key: 'genre', label: 'Genre', weight: 1.3, visible: true },
  { key: 'bpm', label: 'BPM', weight: 0.7, visible: true, numeric: true },
  { key: 'duration', label: 'Dur.', weight: 0.7, visible: true, numeric: true },
];

const COLS_STORAGE = 'sway.columns.v2';
const COVER_COL_W = 56;
const MIN_WEIGHT = 0.4;

function loadCols(): ColDef[] {
  try {
    const saved: Partial<ColDef>[] = JSON.parse(localStorage.getItem(COLS_STORAGE) ?? '');
    // Respeta el orden guardado.
    const byKey = new Map(DEFAULT_COLS.map((d) => [d.key, d]));
    const ordered: ColDef[] = [];
    for (const s of saved) {
      const d = s.key && byKey.get(s.key as ColKey);
      if (d) {
        ordered.push({ ...d, weight: s.weight ?? d.weight, visible: s.visible ?? d.visible });
        byKey.delete(s.key as ColKey);
      }
    }
    for (const d of byKey.values()) ordered.push(d); // columnas nuevas al final
    return ordered.length ? ordered : DEFAULT_COLS;
  } catch {
    return DEFAULT_COLS;
  }
}

function fmt(ms: number): string {
  if (!ms || isNaN(ms)) return '0:00';
  const s = Math.floor(ms / 1000);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

function cellValue(t: Track, key: ColKey): string {
  switch (key) {
    case 'title': return t.title;
    case 'artist': return t.artist;
    case 'album': return t.album;
    case 'genre': return t.genre;
    case 'bpm': return t.bpm != null ? String(t.bpm) : '';
    case 'duration': return fmt(t.durationMs);
  }
}

interface Props {
  tracks: Track[];
  currentId: number | null;
  selected: Set<number>;
  onSelectedChange: (sel: Set<number>) => void;
  onPlay: (id: number) => void;
  onInspect: (id: number) => void;
  canReorder: boolean;
  onTrackMouseDown: (e: React.MouseEvent, ids: number[]) => void;
  dropInsertIndex: number | null;
  wasDrag: () => boolean;
  rowMenuItems: (ids: number[]) => MenuItem[];
}

function TrackTable({
  tracks,
  currentId,
  selected,
  onSelectedChange,
  onPlay,
  onInspect,
  canReorder,
  onTrackMouseDown,
  dropInsertIndex,
  wasDrag,
  rowMenuItems,
}: Props) {
  const [cols, setCols] = useState<ColDef[]>(loadCols);
  const [anchor, setAnchor] = useState<number | null>(null);
  const [rowMenu, setRowMenu] = useState<{ x: number; y: number; ids: number[] } | null>(null);
  const [headMenu, setHeadMenu] = useState<{ x: number; y: number } | null>(null);
  const [colDrag, setColDrag] = useState<{ key: ColKey; overKey: ColKey | null } | null>(null);
  const tableRef = useRef<HTMLTableElement>(null);
  const resizing = useRef<{ key: ColKey; startX: number; startWeight: number; perPx: number } | null>(null);

  useEffect(() => {
    localStorage.setItem(
      COLS_STORAGE,
      JSON.stringify(cols.map(({ key, weight, visible }) => ({ key, weight, visible }))),
    );
  }, [cols]);

  // Resize de columnas (proporcional: cambia el peso => todas se re-reparten).
  useEffect(() => {
    function onMove(e: MouseEvent) {
      const r = resizing.current;
      if (!r) return;
      const w = Math.max(MIN_WEIGHT, r.startWeight + (e.clientX - r.startX) * r.perPx);
      setCols((cs) => cs.map((c) => (c.key === r.key ? { ...c, weight: w } : c)));
    }
    function onUp() {
      resizing.current = null;
      document.body.classList.remove('col-resizing');
    }
    window.addEventListener('mousemove', onMove);
    window.addEventListener('mouseup', onUp);
    return () => {
      window.removeEventListener('mousemove', onMove);
      window.removeEventListener('mouseup', onUp);
    };
  }, []);

  const visibleCols = cols.filter((c) => c.visible);
  const totalWeight = visibleCols.reduce((a, c) => a + c.weight, 0) || 1;

  function startResize(e: React.MouseEvent, c: ColDef) {
    e.preventDefault();
    e.stopPropagation();
    const tableW = (tableRef.current?.clientWidth ?? 800) - COVER_COL_W;
    const pxPerWeight = tableW / totalWeight;
    resizing.current = {
      key: c.key,
      startX: e.clientX,
      startWeight: c.weight,
      perPx: pxPerWeight > 0 ? 1 / pxPerWeight : 0.004,
    };
    document.body.classList.add('col-resizing');
  }

  // Reorden de columnas arrastrando el header.
  function startColDrag(e: React.MouseEvent, key: ColKey) {
    if ((e.target as HTMLElement).closest('.col-handle')) return;
    e.preventDefault();
    const startX = e.clientX;
    let active = false;
    const onMove = (ev: MouseEvent) => {
      if (!active && Math.abs(ev.clientX - startX) < 5) return;
      active = true;
      const th = document.elementFromPoint(ev.clientX, ev.clientY)?.closest<HTMLElement>('th[data-col]');
      const overKey = (th?.dataset.col as ColKey) ?? null;
      setColDrag({ key, overKey: overKey && overKey !== key ? overKey : null });
    };
    const onUp = () => {
      window.removeEventListener('mousemove', onMove);
      window.removeEventListener('mouseup', onUp);
      document.body.classList.remove('col-dragging');
      setColDrag((cd) => {
        if (cd?.overKey) {
          setCols((cs) => {
            const from = cs.findIndex((c) => c.key === cd.key);
            const to = cs.findIndex((c) => c.key === cd.overKey);
            if (from < 0 || to < 0) return cs;
            const next = [...cs];
            const [moved] = next.splice(from, 1);
            next.splice(to, 0, moved);
            return next;
          });
        }
        return null;
      });
    };
    window.addEventListener('mousemove', onMove);
    window.addEventListener('mouseup', onUp);
    document.body.classList.add('col-dragging');
  }

  function onRowClick(e: React.MouseEvent, t: Track, idx: number) {
    if (wasDrag()) return;
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
      onInspect(t.id);
    }
    setAnchor(t.id);
  }

  function onRowContextMenu(e: React.MouseEvent, t: Track) {
    e.preventDefault();
    let ids: number[];
    if (selected.has(t.id)) {
      ids = [...selected];
    } else {
      ids = [t.id];
      onSelectedChange(new Set(ids));
      setAnchor(t.id);
    }
    setRowMenu({ x: e.clientX, y: e.clientY, ids });
  }

  function dragIds(t: Track): number[] {
    return selected.has(t.id) ? [...selected] : [t.id];
  }

  function onKeyDown(e: React.KeyboardEvent) {
    if (e.key === 'Delete' && selected.size > 0) {
      const dangers = rowMenuItems([...selected]).filter((i) => i.danger && !i.disabled);
      const item = e.shiftKey ? dangers[dangers.length - 1] : dangers[0];
      item?.onClick?.();
    }
    if ((e.ctrlKey || e.metaKey) && e.key === 'a') {
      e.preventDefault();
      onSelectedChange(new Set(tracks.map((t) => t.id)));
    }
  }

  return (
    <div className="table-wrap" tabIndex={0} onKeyDown={onKeyDown}>
      <table className="tracks" ref={tableRef}>
        <colgroup>
          <col style={{ width: COVER_COL_W }} />
          {visibleCols.map((c) => (
            <col key={c.key} style={{ width: `${(c.weight / totalWeight) * 100}%` }} />
          ))}
        </colgroup>
        <thead>
          <tr>
            <th className="col-cover"></th>
            {visibleCols.map((c) => (
              <th
                key={c.key}
                data-col={c.key}
                className={[
                  c.numeric ? 'num' : '',
                  colDrag?.key === c.key ? 'col-dragging-src' : '',
                  colDrag?.overKey === c.key ? 'col-drop-target' : '',
                ].join(' ')}
                onMouseDown={(e) => startColDrag(e, c.key)}
                onContextMenu={(e) => {
                  e.preventDefault();
                  setHeadMenu({ x: e.clientX, y: e.clientY });
                }}
              >
                <span className="th-label">{c.label}</span>
                <span className="col-handle" onMouseDown={(e) => startResize(e, c)} />
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {tracks.map((t, idx) => (
            <tr
              key={t.id}
              data-dnd="row"
              data-idx={idx}
              className={[
                t.id === currentId ? 'playing' : '',
                selected.has(t.id) ? 'selected' : '',
                dropInsertIndex === idx ? 'drop-before' : '',
                dropInsertIndex === idx + 1 && idx === tracks.length - 1 ? 'drop-after' : '',
              ].join(' ')}
              onMouseDown={(e) => {
                if ((e.target as HTMLElement).closest('button')) return;
                onTrackMouseDown(e, dragIds(t));
              }}
              onClick={(e) => onRowClick(e, t, idx)}
              onDoubleClick={() => onPlay(t.id)}
              onContextMenu={(e) => onRowContextMenu(e, t)}
            >
              <td className="col-cover">
                <div className="row-cover">
                  <Cover trackId={t.id} />
                  {t.id === currentId ? (
                    <span className="eq" aria-label="Now playing">
                      <i /><i /><i />
                    </span>
                  ) : (
                    <button
                      className="play-ov"
                      onClick={(e) => {
                        e.stopPropagation();
                        onPlay(t.id);
                      }}
                      aria-label="Play"
                    >
                      <Play size={16} fill="currentColor" />
                    </button>
                  )}
                </div>
              </td>
              {visibleCols.map((c) => {
                const val = cellValue(t, c.key);
                return (
                  <td
                    key={c.key}
                    className={[c.numeric ? 'num' : '', c.key === 'title' ? 't-title' : ''].join(' ')}
                    title={val || undefined}
                  >
                    {val}
                  </td>
                );
              })}
            </tr>
          ))}
        </tbody>
      </table>
      {canReorder && tracks.length > 0 && (
        <div
          className={'drop-tail' + (dropInsertIndex === tracks.length ? ' drop-before' : '')}
          data-dnd="tail"
          data-idx={tracks.length}
        />
      )}

      {rowMenu && (
        <ContextMenu
          x={rowMenu.x}
          y={rowMenu.y}
          items={rowMenuItems(rowMenu.ids)}
          onClose={() => setRowMenu(null)}
        />
      )}
      {headMenu && (
        <ContextMenu
          x={headMenu.x}
          y={headMenu.y}
          keepOpen
          items={cols.map((c) => ({
            label: c.label,
            checked: c.visible,
            disabled: c.visible && visibleCols.length === 1,
            onClick: () =>
              setCols((cs) => cs.map((x) => (x.key === c.key ? { ...x, visible: !x.visible } : x))),
          }))}
          onClose={() => setHeadMenu(null)}
        />
      )}
    </div>
  );
}

export default memo(TrackTable);
