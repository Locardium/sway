import { useEffect, useRef, useState } from 'react';
import { Track } from '../api';
import ContextMenu, { MenuItem } from './ContextMenu';

export type ColKey = 'title' | 'artist' | 'album' | 'genre' | 'bpm' | 'duration';

interface ColDef {
  key: ColKey;
  label: string;
  width: number;
  visible: boolean;
  numeric?: boolean;
}

const DEFAULT_COLS: ColDef[] = [
  { key: 'title', label: 'Título', width: 280, visible: true },
  { key: 'artist', label: 'Artista', width: 200, visible: true },
  { key: 'album', label: 'Álbum', width: 200, visible: true },
  { key: 'genre', label: 'Género', width: 140, visible: true },
  { key: 'bpm', label: 'BPM', width: 70, visible: true, numeric: true },
  { key: 'duration', label: 'Dur.', width: 70, visible: true, numeric: true },
];

const COLS_STORAGE = 'sway.columns.v1';
const MIN_W = 60;
const TRACKS_MIME = 'application/x-sway-tracks';

function loadCols(): ColDef[] {
  try {
    const saved: Partial<ColDef>[] = JSON.parse(localStorage.getItem(COLS_STORAGE) ?? '');
    return DEFAULT_COLS.map((d) => {
      const s = saved.find((c) => c.key === d.key);
      return s ? { ...d, width: s.width ?? d.width, visible: s.visible ?? d.visible } : d;
    });
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
  canReorder: boolean;
  onReorder: (trackIds: number[], index: number) => void;
  /** Items extra del menu contextual de fila (los define App). */
  rowMenuItems: (ids: number[]) => MenuItem[];
}

export default function TrackTable({
  tracks,
  currentId,
  selected,
  onSelectedChange,
  onPlay,
  canReorder,
  onReorder,
  rowMenuItems,
}: Props) {
  const [cols, setCols] = useState<ColDef[]>(loadCols);
  const [anchor, setAnchor] = useState<number | null>(null);
  const [dropIdx, setDropIdx] = useState<number | null>(null);
  const [rowMenu, setRowMenu] = useState<{ x: number; y: number; ids: number[] } | null>(null);
  const [headMenu, setHeadMenu] = useState<{ x: number; y: number } | null>(null);
  const resizing = useRef<{ key: ColKey; startX: number; startW: number } | null>(null);

  useEffect(() => {
    localStorage.setItem(
      COLS_STORAGE,
      JSON.stringify(cols.map(({ key, width, visible }) => ({ key, width, visible }))),
    );
  }, [cols]);

  // Resize de columnas: mousedown en el handle, mousemove global.
  useEffect(() => {
    function onMove(e: MouseEvent) {
      const r = resizing.current;
      if (!r) return;
      setCols((cs) =>
        cs.map((c) =>
          c.key === r.key ? { ...c, width: Math.max(MIN_W, r.startW + e.clientX - r.startX) } : c,
        ),
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

  function onRowContextMenu(e: React.MouseEvent, t: Track) {
    e.preventDefault();
    // Click derecho sobre fila no seleccionada: la selecciona (comportamiento estándar).
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

  function rowDropIndex(e: React.DragEvent, idx: number): number {
    const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
    return e.clientY - r.top < r.height / 2 ? idx : idx + 1;
  }

  function onKeyDown(e: React.KeyboardEvent) {
    if (e.key === 'Delete' && selected.size > 0) {
      const del = rowMenuItems([...selected]).find((i) => i.danger && !i.disabled);
      del?.onClick?.();
    }
    if ((e.ctrlKey || e.metaKey) && e.key === 'a') {
      e.preventDefault();
      onSelectedChange(new Set(tracks.map((t) => t.id)));
    }
  }

  return (
    <div className="table-wrap" tabIndex={0} onKeyDown={onKeyDown} data-drop="library">
      <table className="tracks" style={{ width: 34 + visibleCols.reduce((a, c) => a + c.width, 0) }}>
        <colgroup>
          <col style={{ width: 34 }} />
          {visibleCols.map((c) => (
            <col key={c.key} style={{ width: c.width }} />
          ))}
        </colgroup>
        <thead>
          <tr
            onContextMenu={(e) => {
              e.preventDefault();
              setHeadMenu({ x: e.clientX, y: e.clientY });
            }}
          >
            <th className="col-play"></th>
            {visibleCols.map((c) => (
              <th key={c.key} className={c.numeric ? 'num' : ''}>
                {c.label}
                <span
                  className="col-handle"
                  onMouseDown={(e) => {
                    e.preventDefault();
                    resizing.current = { key: c.key, startX: e.clientX, startW: c.width };
                    document.body.classList.add('col-resizing');
                  }}
                />
              </th>
            ))}
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
              onContextMenu={(e) => onRowContextMenu(e, t)}
            >
              <td className="col-play">
                <button className="mini" onClick={(e) => { e.stopPropagation(); onPlay(t.id); }}>
                  {t.id === currentId ? '♫' : '▶'}
                </button>
              </td>
              {visibleCols.map((c) => (
                <td
                  key={c.key}
                  className={[c.numeric ? 'num' : '', c.key === 'title' ? 't-title' : ''].join(' ')}
                >
                  {cellValue(t, c.key)}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
      {/* Zona de drop debajo de la última fila: inserta al final. */}
      {canReorder && tracks.length > 0 && (
        <div
          className={'drop-tail' + (dropIdx === tracks.length ? ' drop-before' : '')}
          onDragOver={(e) => {
            if (e.dataTransfer.types.includes(TRACKS_MIME)) {
              e.preventDefault();
              setDropIdx(tracks.length);
            }
          }}
          onDragLeave={() => setDropIdx((d) => (d === tracks.length ? null : d))}
          onDrop={(e) => {
            e.preventDefault();
            const data = e.dataTransfer.getData(TRACKS_MIME);
            setDropIdx(null);
            if (data) onReorder(JSON.parse(data), tracks.length);
          }}
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
            // La última columna visible no se puede ocultar.
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
