import { memo, useEffect, useRef, useState } from 'react';
import { Play } from 'lucide-react';
import { Track } from '../api';
import ContextMenu, { MenuItem } from './ContextMenu';
import { useUiSetting } from '../uiSettings';
import Cover from './Cover';

export type ColKey = 'title' | 'artist' | 'album' | 'genre' | 'bpm' | 'duration';

/// Which edge of the hovered header a dragged column will be dropped on.
type DropSide = 'before' | 'after';

interface ColDef {
  key: ColKey;
  label: string;
  weight: number; // relative width (proportional to the viewport)
  visible: boolean;
  numeric?: boolean;
}

const DEFAULT_COLS: ColDef[] = [
  { key: 'title', label: 'Title', weight: 3, visible: true },
  { key: 'artist', label: 'Artist', weight: 2, visible: true },
  { key: 'album', label: 'Album', weight: 2, visible: true },
  { key: 'genre', label: 'Genre', weight: 1.3, visible: true },
  { key: 'bpm', label: 'BPM', weight: 0.7, visible: true, numeric: true },
  { key: 'duration', label: 'Duration', weight: 0.7, visible: true, numeric: true },
];

/// Where the layout lives now: a db setting, so the file is `<Music>/Sway`.
const COLS_SETTING = 'columns.v2';
/// Where it used to live: the webview's `localStorage`, which sits in the
/// browser profile under AppData. Read once on startup to carry an existing
/// layout across, then deleted. Can go once no install predates the move.
const COLS_LEGACY_STORAGE = 'sway.columns.v2';
// Art (38) plus the table cell padding on both sides (12 + 12), so the first
// text column starts on the same grid as every other cell.
const COVER_COL_W = 62;
const MIN_WEIGHT = 0.4;

function parseCols(raw: string | null): ColDef[] | null {
  if (!raw) return null;
  try {
    const saved: Partial<ColDef>[] = JSON.parse(raw);
    // Respects the saved order.
    const byKey = new Map(DEFAULT_COLS.map((d) => [d.key, d]));
    const ordered: ColDef[] = [];
    for (const s of saved) {
      const d = s.key && byKey.get(s.key as ColKey);
      if (d) {
        ordered.push({ ...d, weight: s.weight ?? d.weight, visible: s.visible ?? d.visible });
        byKey.delete(s.key as ColKey);
      }
    }
    for (const d of byKey.values()) ordered.push(d); // new columns go at the end
    return ordered.length ? ordered : null;
  } catch {
    return null;
  }
}

function serializeCols(cols: ColDef[]): string {
  return JSON.stringify(cols.map(({ key, weight, visible }) => ({ key, weight, visible })));
}

/// Whether the columns are already exactly as they ship: same order, same
/// widths, same ones visible. Drives whether "Reset columns" has anything to
/// do. Weights are compared with a tolerance because a resize leaves floats.
function isDefaultCols(cols: ColDef[]): boolean {
  if (cols.length !== DEFAULT_COLS.length) return false;
  return cols.every((c, i) => {
    const d = DEFAULT_COLS[i];
    return c.key === d.key && c.visible === d.visible && Math.abs(c.weight - d.weight) < 0.001;
  });
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
  tapToPlay?: boolean;
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
  tapToPlay = false,
}: Props) {
  // Order, widths and visibility, stored in the db (so: `<Music>/Sway`) and
  // carried across from the webview's localStorage on first run after the
  // move. The hook owns the read-then-write ordering and the throttling.
  const [cols, setCols] = useUiSetting<ColDef[]>(COLS_SETTING, DEFAULT_COLS, parseCols, serializeCols, {
    legacyKey: COLS_LEGACY_STORAGE,
  });
  const [anchor, setAnchor] = useState<number | null>(null);
  const [rowMenu, setRowMenu] = useState<{ x: number; y: number; ids: number[] } | null>(null);
  const [headMenu, setHeadMenu] = useState<{ x: number; y: number } | null>(null);
  const [colDrag, setColDrag] = useState<{
    key: ColKey;
    overKey: ColKey | null;
    side: DropSide;
  } | null>(null);
  const tableRef = useRef<HTMLTableElement>(null);
  const resizing = useRef<{
    key: ColKey;
    /// The column on the other side of the divider being dragged. Width is
    /// transferred between the two, never taken from the table as a whole.
    nextKey: ColKey;
    startX: number;
    startWeight: number;
    startNextWeight: number;
    perPx: number;
  } | null>(null);

  // Column resize. Dragging a divider moves width between the two columns it
  // separates and nothing else: their combined weight is held constant, so
  // the total stays put and every other column keeps the exact position it
  // had. Growing one column used to raise the total, which re-scaled all the
  // others — including the ones to the LEFT of the divider, which is not
  // something a resize should ever touch.
  useEffect(() => {
    function onMove(e: MouseEvent) {
      const r = resizing.current;
      if (!r) return;
      const pair = r.startWeight + r.startNextWeight;
      const w = Math.min(
        pair - MIN_WEIGHT,
        Math.max(MIN_WEIGHT, r.startWeight + (e.clientX - r.startX) * r.perPx),
      );
      setCols((cs) =>
        cs.map((c) => {
          if (c.key === r.key) return { ...c, weight: w };
          if (c.key === r.nextKey) return { ...c, weight: pair - w };
          return c;
        }),
      );
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

  function startResize(e: React.MouseEvent, c: ColDef, idx: number) {
    const next = visibleCols[idx + 1];
    // The last column has no neighbour to trade with, so its right edge is
    // the table's edge and there is nothing to drag. No handle is rendered
    // there; this is the guard for it.
    if (!next) return;
    e.preventDefault();
    e.stopPropagation();
    const tableW = (tableRef.current?.clientWidth ?? 800) - COVER_COL_W;
    const pxPerWeight = tableW / totalWeight;
    resizing.current = {
      key: c.key,
      nextKey: next.key,
      startX: e.clientX,
      startWeight: c.weight,
      startNextWeight: next.weight,
      // Holding the pair's sum constant keeps totalWeight fixed for the whole
      // gesture, so this pixels-to-weight ratio stays true from first move to
      // release instead of drifting under the pointer.
      perPx: pxPerWeight > 0 ? 1 / pxPerWeight : 0.004,
    };
    document.body.classList.add('col-resizing');
  }

  // Reorder columns by dragging the header.
  function startColDrag(e: React.MouseEvent, key: ColKey) {
    if ((e.target as HTMLElement).closest('.col-handle')) return;
    // Only the left button drags; a right-click has to reach the header menu.
    if (e.button !== 0) return;
    e.preventDefault();
    const startX = e.clientX;
    let active = false;
    const onMove = (ev: MouseEvent) => {
      if (!active && Math.abs(ev.clientX - startX) < 5) return;
      if (!active) {
        active = true;
        // Only once the press has turned into a real drag: adding this on
        // mousedown made every plain click flash the grabbing cursor.
        document.body.classList.add('col-dragging');
      }
      const th = document.elementFromPoint(ev.clientX, ev.clientY)?.closest<HTMLElement>('th[data-col]');
      const overKey = (th?.dataset.col as ColKey) ?? null;
      if (!th || !overKey || overKey === key) {
        setColDrag({ key, overKey: null, side: 'before' });
        return;
      }
      // Which half of the target the pointer is over decides which side the
      // column lands on, so a drop next to the last column is reachable.
      const r = th.getBoundingClientRect();
      const side: DropSide = ev.clientX < r.left + r.width / 2 ? 'before' : 'after';
      setColDrag({ key, overKey, side });
    };
    const onUp = () => {
      window.removeEventListener('mousemove', onMove);
      window.removeEventListener('mouseup', onUp);
      document.body.classList.remove('col-dragging');
      setColDrag((cd) => {
        if (cd?.overKey) {
          setCols((cs) => {
            const from = cs.findIndex((c) => c.key === cd.key);
            const target = cs.findIndex((c) => c.key === cd.overKey);
            if (from < 0 || target < 0) return cs;
            let to = cd.side === 'after' ? target + 1 : target;
            const next = [...cs];
            const [moved] = next.splice(from, 1);
            // Removing the column first shifts every later index down by one,
            // so an insertion point past it has to come down with them.
            // Without this, dragging a column one place to the right put it
            // back exactly where it started.
            if (from < to) to -= 1;
            next.splice(to, 0, moved);
            return next;
          });
        }
        return null;
      });
    };
    window.addEventListener('mousemove', onMove);
    window.addEventListener('mouseup', onUp);
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
      // On Android there's no double-tap or right-click for "Play" — a single
      // tap already plays (see App.tsx, tapToPlay prop).
      if (tapToPlay) onPlay(t.id);
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
            {visibleCols.map((c, i) => (
              <th
                key={c.key}
                data-col={c.key}
                className={[
                  c.numeric ? 'num' : '',
                  colDrag?.key === c.key ? 'col-dragging-src' : '',
                  colDrag?.overKey === c.key ? `col-drop-${colDrag.side}` : '',
                ].join(' ')}
                onMouseDown={(e) => startColDrag(e, c.key)}
                onContextMenu={(e) => {
                  e.preventDefault();
                  setHeadMenu({ x: e.clientX, y: e.clientY });
                }}
              >
                <span className="th-label">{c.label}</span>
                {/* No divider after the last column: its right edge is the
                    table's own, and there is no neighbour to trade width
                    with. Drawing a grabbable line there would be a lie. */}
                {i < visibleCols.length - 1 && (
                  <span className="col-handle" onMouseDown={(e) => startResize(e, c, i)} />
                )}
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
                // No file on this device (selective sync): it still shows up
                // and can be organized, but it won't play.
                t.present === false ? 'absent' : '',
                // Its playlist isn't checked for this device: the file that's
                // already here is left alone, but it won't sync anymore.
                t.inScope === false ? 'out-scope' : '',
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
          items={[
            ...cols.map((c) => ({
              label: c.label,
              checked: c.visible,
              disabled: c.visible && visibleCols.length === 1,
              onClick: () =>
                setCols((cs) =>
                  cs.map((x) => (x.key === c.key ? { ...x, visible: !x.visible } : x)),
                ),
            })),
            { separator: true, label: '' },
            {
              label: 'Reset columns',
              // Order, widths and visibility all come back at once — they are
              // one saved thing, and resetting half of it would be a puzzle.
              disabled: isDefaultCols(cols),
              onClick: () => {
                // Copied, not the module array itself, so the defaults can
                // never be reached by a later edit.
                setCols(DEFAULT_COLS.map((c) => ({ ...c })));
                // This menu is keepOpen (for the checkboxes); the reset is a
                // one-shot action, so it closes itself.
                setHeadMenu(null);
              },
            },
          ]}
          onClose={() => setHeadMenu(null)}
        />
      )}
    </div>
  );
}

export default memo(TrackTable);
