import { useEffect, useRef, useState } from 'react';
import { PlaylistNode, NodeKind } from '../api';

export type Selection = { type: 'library' } | { type: 'playlist'; id: number };

interface Props {
  nodes: PlaylistNode[];
  selection: Selection;
  /** Nodo resaltado durante un drag de archivos del OS (null = ninguno). */
  osDropNodeId: number | null;
  busy: boolean;
  onSelect: (sel: Selection) => void;
  onImport: () => void;
  onCreate: (kind: NodeKind, parentId: number | null) => void;
  onRename: (id: number, name: string) => void;
  onDelete: (id: number) => void;
  onMoveNode: (id: number, parentId: number | null, index: number) => void;
  onDropTracks: (playlistId: number, trackIds: number[]) => void;
}

type DropHint = { nodeId: number; zone: 'before' | 'after' | 'into' } | null;

const TRACKS_MIME = 'application/x-sway-tracks';
const NODE_MIME = 'application/x-sway-node';

export default function Sidebar({
  nodes,
  selection,
  osDropNodeId,
  busy,
  onSelect,
  onImport,
  onCreate,
  onRename,
  onDelete,
  onMoveNode,
  onDropTracks,
}: Props) {
  const [collapsed, setCollapsed] = useState<Set<number>>(new Set());
  const [editingId, setEditingId] = useState<number | null>(null);
  const [menu, setMenu] = useState<{ x: number; y: number; node: PlaylistNode | null } | null>(null);
  const [dropHint, setDropHint] = useState<DropHint>(null);
  const [rootHover, setRootHover] = useState(false);
  const editRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (editingId != null) editRef.current?.select();
  }, [editingId]);

  useEffect(() => {
    if (!menu) return;
    const close = () => setMenu(null);
    window.addEventListener('click', close);
    window.addEventListener('contextmenu', close, true);
    return () => {
      window.removeEventListener('click', close);
      window.removeEventListener('contextmenu', close, true);
    };
  }, [menu]);

  const childrenOf = (pid: number | null) =>
    nodes.filter((n) => n.parentId === pid).sort((a, b) => a.position - b.position);

  function toggleCollapse(id: number) {
    setCollapsed((prev) => {
      const next = new Set(prev);
      next.has(id) ? next.delete(id) : next.add(id);
      return next;
    });
  }

  function zoneFor(e: React.DragEvent, node: PlaylistNode): 'before' | 'after' | 'into' {
    const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
    const y = e.clientY - r.top;
    if (node.kind === 'folder') {
      if (y < r.height * 0.25) return 'before';
      if (y > r.height * 0.75) return 'after';
      return 'into';
    }
    return y < r.height / 2 ? 'before' : 'after';
  }

  function onRowDragOver(e: React.DragEvent, node: PlaylistNode) {
    const types = e.dataTransfer.types;
    if (types.includes(TRACKS_MIME)) {
      if (node.kind === 'playlist') {
        e.preventDefault();
        setDropHint({ nodeId: node.id, zone: 'into' });
      }
      return;
    }
    if (types.includes(NODE_MIME)) {
      e.preventDefault();
      setDropHint({ nodeId: node.id, zone: zoneFor(e, node) });
    }
  }

  function onRowDrop(e: React.DragEvent, node: PlaylistNode) {
    e.preventDefault();
    setDropHint(null);
    const trackData = e.dataTransfer.getData(TRACKS_MIME);
    if (trackData && node.kind === 'playlist') {
      onDropTracks(node.id, JSON.parse(trackData));
      return;
    }
    const nodeData = e.dataTransfer.getData(NODE_MIME);
    if (!nodeData) return;
    const dragId = Number(nodeData);
    if (dragId === node.id) return;
    const zone = zoneFor(e, node);
    if (zone === 'into') {
      onMoveNode(dragId, node.id, childrenOf(node.id).length);
    } else {
      const siblings = childrenOf(node.parentId);
      let idx = siblings.findIndex((s) => s.id === node.id);
      if (zone === 'after') idx += 1;
      onMoveNode(dragId, node.parentId, idx);
    }
  }

  function renderNode(node: PlaylistNode, depth: number) {
    const isFolder = node.kind === 'folder';
    const kids = isFolder ? childrenOf(node.id) : [];
    const isCollapsed = collapsed.has(node.id);
    const active = selection.type === 'playlist' && selection.id === node.id;
    const hint = dropHint?.nodeId === node.id ? dropHint.zone : null;

    return (
      <div key={node.id}>
        <div
          className={[
            'tree-row',
            active ? 'active' : '',
            hint ? `drop-${hint}` : '',
            osDropNodeId === node.id ? 'drop-into' : '',
          ].join(' ')}
          style={{ paddingLeft: 10 + depth * 16 }}
          data-drop-node={node.id}
          data-node-kind={node.kind}
          draggable={editingId !== node.id}
          onDragStart={(e) => {
            e.dataTransfer.setData(NODE_MIME, String(node.id));
            e.dataTransfer.effectAllowed = 'move';
          }}
          onDragOver={(e) => onRowDragOver(e, node)}
          onDragLeave={() => setDropHint((h) => (h?.nodeId === node.id ? null : h))}
          onDrop={(e) => onRowDrop(e, node)}
          onClick={() => {
            if (isFolder) toggleCollapse(node.id);
            else onSelect({ type: 'playlist', id: node.id });
          }}
          onDoubleClick={() => setEditingId(node.id)}
          onContextMenu={(e) => {
            e.preventDefault();
            e.stopPropagation();
            setMenu({ x: e.clientX, y: e.clientY, node });
          }}
        >
          <span className="tree-icon">
            {isFolder ? (isCollapsed ? '▸' : '▾') : '♪'}
          </span>
          {editingId === node.id ? (
            <input
              ref={editRef}
              className="tree-edit"
              defaultValue={node.name}
              onClick={(e) => e.stopPropagation()}
              onBlur={(e) => {
                const v = e.target.value.trim();
                if (v && v !== node.name) onRename(node.id, v);
                setEditingId(null);
              }}
              onKeyDown={(e) => {
                if (e.key === 'Enter') (e.target as HTMLInputElement).blur();
                if (e.key === 'Escape') setEditingId(null);
              }}
            />
          ) : (
            <span className="tree-name">{node.name}</span>
          )}
          {!isFolder && <span className="tree-count">{node.trackCount || ''}</span>}
        </div>
        {isFolder && !isCollapsed && kids.map((k) => renderNode(k, depth + 1))}
      </div>
    );
  }

  return (
    <aside className="sidebar">
      <div
        className={'tree-row library' + (selection.type === 'library' ? ' active' : '')}
        onClick={() => onSelect({ type: 'library' })}
      >
        <span className="tree-icon">🗄</span>
        <span className="tree-name">Biblioteca</span>
      </div>

      <div className="tree-head">
        <span>PLAYLISTS</span>
        <div className="tree-actions">
          <button title="Nueva playlist" onClick={() => onCreate('playlist', null)}>
            +
          </button>
          <button title="Nueva carpeta" onClick={() => onCreate('folder', null)}>
            🗀
          </button>
        </div>
      </div>

      <div
        className={'tree' + (rootHover ? ' drop-root' : '')}
        onContextMenu={(e) => {
          e.preventDefault();
          setMenu({ x: e.clientX, y: e.clientY, node: null });
        }}
        onDragOver={(e) => {
          // Solo si el drag no fue capturado por una fila (drop al final de la raiz).
          if (e.dataTransfer.types.includes(NODE_MIME) && e.target === e.currentTarget) {
            e.preventDefault();
            setRootHover(true);
          }
        }}
        onDragLeave={(e) => {
          if (e.target === e.currentTarget) setRootHover(false);
        }}
        onDrop={(e) => {
          if (e.target !== e.currentTarget) return;
          e.preventDefault();
          setRootHover(false);
          const nodeData = e.dataTransfer.getData(NODE_MIME);
          if (nodeData) onMoveNode(Number(nodeData), null, childrenOf(null).length);
        }}
      >
        {childrenOf(null).map((n) => renderNode(n, 0))}
        {nodes.length === 0 && <p className="tree-empty">Sin playlists todavía.</p>}
      </div>

      <button className="import-btn" onClick={onImport} disabled={busy}>
        {busy ? 'Importando…' : '+ Importar carpeta'}
      </button>

      {menu && (
        <div className="ctx-menu" style={{ left: menu.x, top: menu.y }}>
          {menu.node ? (
            <>
              {menu.node.kind === 'folder' && (
                <>
                  <button onClick={() => onCreate('playlist', menu.node!.id)}>Nueva playlist acá</button>
                  <button onClick={() => onCreate('folder', menu.node!.id)}>Nueva carpeta acá</button>
                  <hr />
                </>
              )}
              <button onClick={() => setEditingId(menu.node!.id)}>Renombrar</button>
              <button className="danger" onClick={() => onDelete(menu.node!.id)}>
                Eliminar
              </button>
            </>
          ) : (
            <>
              <button onClick={() => onCreate('playlist', null)}>Nueva playlist</button>
              <button onClick={() => onCreate('folder', null)}>Nueva carpeta</button>
            </>
          )}
        </div>
      )}
    </aside>
  );
}
