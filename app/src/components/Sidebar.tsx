import { useEffect, useMemo, useRef, useState } from 'react';
import { ChevronRight, FolderPlus, Library, ListMusic, Plus } from 'lucide-react';
import { PlaylistNode, NodeKind } from '../api';

export type Selection = { type: 'library' } | { type: 'playlist'; id: number };

export type NodeDropHint = { nodeId: number; zone: 'before' | 'after' | 'into' } | null;

interface Props {
  nodes: PlaylistNode[];
  selection: Selection;
  /** Hint de drop activo (drag interno o de archivos del OS). */
  dropHint: NodeDropHint;
  rootHover: boolean;
  onSelect: (sel: Selection) => void;
  onCreate: (kind: NodeKind, parentId: number | null) => void;
  onRename: (id: number, name: string) => void;
  onDelete: (id: number) => void;
  /** mousedown que puede iniciar drag de un nodo (App + dnd.ts). */
  onNodeMouseDown: (e: React.MouseEvent, id: number) => void;
  wasDrag: () => boolean;
}

export default function Sidebar({
  nodes,
  selection,
  dropHint,
  rootHover,
  onSelect,
  onCreate,
  onRename,
  onDelete,
  onNodeMouseDown,
  wasDrag,
}: Props) {
  const [collapsed, setCollapsed] = useState<Set<number>>(new Set());
  const [editingId, setEditingId] = useState<number | null>(null);
  const [menu, setMenu] = useState<{ x: number; y: number; node: PlaylistNode | null } | null>(null);
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

  // Hijos por padre, armados una vez. Filtrar la lista entera por cada nodo
  // mientras se dibuja el árbol es cuadrático, y el árbol se redibuja en cada
  // cambio de la biblioteca — en el celular eso se siente.
  const byParent = useMemo(() => {
    const m = new Map<number | null, PlaylistNode[]>();
    for (const n of nodes) {
      const list = m.get(n.parentId);
      list ? list.push(n) : m.set(n.parentId, [n]);
    }
    for (const list of m.values()) list.sort((a, b) => a.position - b.position);
    return m;
  }, [nodes]);
  const childrenOf = (pid: number | null) => byParent.get(pid) ?? [];

  function toggleCollapse(id: number) {
    setCollapsed((prev) => {
      const next = new Set(prev);
      next.has(id) ? next.delete(id) : next.add(id);
      return next;
    });
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
            // Desmarcada pero todavía ocupando lugar. Sigue en el árbol hasta
            // que se libere el espacio; apagada para que se vea que está de
            // salida.
            node.inScope ? '' : 'out-scope',
          ].join(' ')}
          title={node.inScope ? undefined : 'Not synced to this device — still taking up space'}
          style={{ paddingLeft: 10 + depth * 15 }}
          data-dnd="node"
          data-node-id={node.id}
          data-node-kind={node.kind}
          onMouseDown={(e) => {
            if (editingId === node.id || (e.target as HTMLElement).closest('input')) return;
            onNodeMouseDown(e, node.id);
          }}
          onClick={() => {
            if (wasDrag()) return;
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
          <span className={'tree-icon' + (isFolder && !isCollapsed ? ' open' : '')}>
            {isFolder ? <ChevronRight size={13} /> : <ListMusic size={13} />}
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
          {/* En una desmarcada el número es el de lo que todavía tiene archivo
              acá, que es lo que se va a ver al abrirla. El total entero diría
              un número que no coincide con la lista. */}
          {!isFolder && (
            <span className="tree-count">
              {(node.inScope ? node.trackCount : node.presentCount) || ''}
            </span>
          )}
        </div>
        <div className={'tree-kids' + (isCollapsed ? ' closed' : '')}>
          {isFolder && !isCollapsed && kids.map((k) => renderNode(k, depth + 1))}
        </div>
      </div>
    );
  }

  return (
    <aside className="sidebar">
      <div
        className={'tree-row library' + (selection.type === 'library' ? ' active' : '')}
        onClick={() => onSelect({ type: 'library' })}
      >
        <span className="tree-icon"><Library size={14} /></span>
        <span className="tree-name">Library</span>
      </div>

      <div className="tree-head">
        <span>Playlists</span>
        <div className="tree-actions">
          <button title="New playlist" onClick={() => onCreate('playlist', null)}>
            <Plus size={14} />
          </button>
          <button title="New folder" onClick={() => onCreate('folder', null)}>
            <FolderPlus size={14} />
          </button>
        </div>
      </div>

      <div
        className={'tree' + (rootHover ? ' drop-root' : '')}
        data-dnd="root"
        onContextMenu={(e) => {
          e.preventDefault();
          setMenu({ x: e.clientX, y: e.clientY, node: null });
        }}
      >
        {childrenOf(null).map((n) => renderNode(n, 0))}
        {nodes.length === 0 && (
          <p className="tree-empty">No playlists yet. Create one with +.</p>
        )}
      </div>

      {menu && (
        <div className="ctx-menu" style={{ left: menu.x, top: menu.y }}>
          {menu.node ? (
            <>
              {menu.node.kind === 'folder' && (
                <>
                  <button onClick={() => onCreate('playlist', menu.node!.id)}>New playlist here</button>
                  <button onClick={() => onCreate('folder', menu.node!.id)}>New folder here</button>
                  <hr />
                </>
              )}
              <button onClick={() => setEditingId(menu.node!.id)}>Rename</button>
              <button className="danger" onClick={() => onDelete(menu.node!.id)}>
                Delete
              </button>
            </>
          ) : (
            <>
              <button onClick={() => onCreate('playlist', null)}>New playlist</button>
              <button onClick={() => onCreate('folder', null)}>New folder</button>
            </>
          )}
        </div>
      )}
    </aside>
  );
}
