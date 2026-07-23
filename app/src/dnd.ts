// Drag & drop propio con pointer events. HTML5 DnD no funciona dentro del
// webview de Tauri cuando dragDropEnabled esta activo (necesario para recibir
// archivos del OS), asi que el DnD interno se hace a mano.
//
// Los targets se anotan en el DOM:
//   data-dnd="row"  data-idx  — fila de la tabla (insertar antes/despues)
//   data-dnd="tail" data-idx  — zona despues de la ultima fila
//   data-dnd="node" data-node-id data-node-kind — nodo del sidebar
//   data-dnd="root"           — raiz del arbol de playlists

export type DragPayload =
  | { kind: 'tracks'; ids: number[]; label: string }
  | { kind: 'node'; id: number; label: string };

export type RawTarget =
  | { type: 'insert'; index: number }
  | { type: 'node'; id: number; nodeKind: string; zone: 'before' | 'after' | 'into' }
  | { type: 'root' }
  | null;

interface Handlers {
  onHover: (payload: DragPayload, target: RawTarget) => void;
  onDrop: (payload: DragPayload, target: RawTarget) => void;
  onEnd: () => void;
}

const THRESHOLD = 5;

/** True mientras (o justo despues de que) un drag consumio el gesto: los
 *  onClick de las filas lo consultan para no disparar seleccion. */
export let didDrag = false;

export function beginDrag(e: React.MouseEvent, payload: DragPayload, handlers: Handlers) {
  if (e.button !== 0) return;
  const startX = e.clientX;
  const startY = e.clientY;
  let active = false;
  let ghost: HTMLDivElement | null = null;
  let lastTarget: RawTarget = null;
  let scrollRaf = 0;
  didDrag = false;

  function resolveTarget(x: number, y: number): RawTarget {
    const el = document.elementFromPoint(x, y)?.closest<HTMLElement>('[data-dnd]');
    if (!el) return null;
    const kind = el.dataset.dnd;
    if (kind === 'row') {
      const idx = Number(el.dataset.idx);
      const r = el.getBoundingClientRect();
      return { type: 'insert', index: y - r.top < r.height / 2 ? idx : idx + 1 };
    }
    if (kind === 'tail') {
      return { type: 'insert', index: Number(el.dataset.idx) };
    }
    if (kind === 'node') {
      const r = el.getBoundingClientRect();
      const frac = (y - r.top) / r.height;
      const nodeKind = el.dataset.nodeKind ?? 'playlist';
      let zone: 'before' | 'after' | 'into';
      if (payload.kind === 'tracks') {
        zone = 'into';
      } else if (nodeKind === 'folder') {
        zone = frac < 0.25 ? 'before' : frac > 0.75 ? 'after' : 'into';
      } else {
        zone = frac < 0.5 ? 'before' : 'after';
      }
      return { type: 'node', id: Number(el.dataset.nodeId), nodeKind, zone };
    }
    if (kind === 'root') return { type: 'root' };
    return null;
  }

  // Auto-scroll cuando el cursor esta cerca del borde de un contenedor scrolleable.
  function autoScroll(x: number, y: number) {
    cancelAnimationFrame(scrollRaf);
    const scroller = document
      .elementFromPoint(x, y)
      ?.closest<HTMLElement>('.table-wrap, .sidebar');
    if (!scroller) return;
    const r = scroller.getBoundingClientRect();
    const M = 44;
    let dy = 0;
    if (y < r.top + M) dy = -Math.ceil((r.top + M - y) / 4);
    else if (y > r.bottom - M) dy = Math.ceil((y - (r.bottom - M)) / 4);
    if (dy !== 0) {
      const step = () => {
        scroller.scrollTop += dy;
        scrollRaf = requestAnimationFrame(step);
      };
      scrollRaf = requestAnimationFrame(step);
    }
  }

  function onMove(ev: MouseEvent) {
    if (!active) {
      if (Math.abs(ev.clientX - startX) + Math.abs(ev.clientY - startY) < THRESHOLD) return;
      active = true;
      didDrag = true;
      ghost = document.createElement('div');
      ghost.className = 'dnd-ghost';
      ghost.textContent = payload.label;
      document.body.appendChild(ghost);
      document.body.classList.add('dragging');
    }
    ghost!.style.transform = `translate(${ev.clientX + 14}px, ${ev.clientY + 10}px)`;
    lastTarget = resolveTarget(ev.clientX, ev.clientY);
    handlers.onHover(payload, lastTarget);
    autoScroll(ev.clientX, ev.clientY);
  }

  function cleanup() {
    window.removeEventListener('mousemove', onMove);
    window.removeEventListener('mouseup', onUp);
    window.removeEventListener('keydown', onKey);
    cancelAnimationFrame(scrollRaf);
    ghost?.remove();
    document.body.classList.remove('dragging');
    handlers.onEnd();
    // didDrag se apaga en el proximo tick para que el click posterior lo vea.
    setTimeout(() => {
      didDrag = false;
    }, 0);
  }

  function onUp(ev: MouseEvent) {
    if (active) {
      handlers.onDrop(payload, resolveTarget(ev.clientX, ev.clientY));
    }
    cleanup();
  }

  function onKey(ev: KeyboardEvent) {
    if (ev.key === 'Escape') cleanup();
  }

  window.addEventListener('mousemove', onMove);
  window.addEventListener('mouseup', onUp);
  window.addEventListener('keydown', onKey);
}
