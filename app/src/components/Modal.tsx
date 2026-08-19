import { ReactNode, useEffect, useRef, useState } from 'react';
import { X } from 'lucide-react';

const EXIT_MS = 180;

interface ModalProps {
  title: string;
  onClose: () => void;
  children: ReactNode;
  wide?: boolean;
}

export function Modal({ title, onClose, children, wide }: ModalProps) {
  const [closing, setClosing] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout>>();

  // Plays the exit animation and only unmounts afterwards (via onClose).
  function close() {
    if (closing) return;
    setClosing(true);
    timer.current = setTimeout(onClose, EXIT_MS);
  }

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') close();
    };
    window.addEventListener('keydown', onKey);
    return () => {
      window.removeEventListener('keydown', onKey);
      clearTimeout(timer.current);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // The phone's keyboard covers the field you're typing into.
  //
  // `adjustResize` in the manifest shrinks the window, but that alone isn't
  // enough: the field can still end up below the cut, so you'd be typing
  // blind. This brings it to the center of what's left visible.
  //
  // There's a delay because the keyboard takes time to appear: requesting the
  // scroll before the window shrinks measures against the old height, i.e.
  // doesn't move at all. And it repeats whenever `visualViewport` reports an
  // actual change, which covers slow keyboards and ones that grow when the
  // suggestion bar appears.
  const modalRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const node = modalRef.current;
    if (!node) return;

    const bring = () => {
      const el = document.activeElement;
      if (!(el instanceof HTMLElement) || !node.contains(el)) return;
      if (!el.matches('input, textarea, [contenteditable]')) return;
      el.scrollIntoView({ block: 'center', behavior: 'smooth' });
    };
    const onFocus = () => setTimeout(bring, 300);

    node.addEventListener('focusin', onFocus);
    const vv = window.visualViewport;
    vv?.addEventListener('resize', bring);
    return () => {
      node.removeEventListener('focusin', onFocus);
      vv?.removeEventListener('resize', bring);
    };
  }, []);

  return (
    <div
      className={'modal-backdrop' + (closing ? ' closing' : '')}
      onMouseDown={(e) => e.target === e.currentTarget && close()}
    >
      <div
        ref={modalRef}
        className={'modal' + (wide ? ' wide' : '') + (closing ? ' closing' : '')}
        role="dialog"
        aria-label={title}
      >
        <div className="modal-head">
          <h3>{title}</h3>
          <button className="mini" onClick={close} aria-label="Close"><X size={15} /></button>
        </div>
        {children}
      </div>
    </div>
  );
}

interface NamePromptProps {
  title: string;
  placeholder: string;
  initial?: string;
  submitLabel: string;
  onSubmit: (name: string) => void;
  onClose: () => void;
}

export function NamePrompt({ title, placeholder, initial, submitLabel, onSubmit, onClose }: NamePromptProps) {
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => ref.current?.select(), []);

  function submit() {
    const v = ref.current?.value.trim();
    if (v) {
      onSubmit(v);
      onClose();
    }
  }

  return (
    <Modal title={title} onClose={onClose}>
      <input
        ref={ref}
        className="modal-input"
        placeholder={placeholder}
        defaultValue={initial ?? ''}
        onKeyDown={(e) => e.key === 'Enter' && submit()}
      />
      <div className="modal-actions">
        <button onClick={onClose}>Cancel</button>
        <button className="primary" onClick={submit}>{submitLabel}</button>
      </div>
    </Modal>
  );
}

interface ConfirmProps {
  title: string;
  message: string;
  confirmLabel: string;
  onConfirm: () => void;
  onClose: () => void;
}

export function Confirm({ title, message, confirmLabel, onConfirm, onClose }: ConfirmProps) {
  return (
    <Modal title={title} onClose={onClose}>
      <p className="modal-msg">{message}</p>
      <div className="modal-actions">
        <button onClick={onClose}>Cancel</button>
        <button
          className="danger-btn"
          autoFocus
          onClick={() => {
            onConfirm();
            onClose();
          }}
        >
          {confirmLabel}
        </button>
      </div>
    </Modal>
  );
}
