import { ReactNode, useEffect, useRef } from 'react';

interface ModalProps {
  title: string;
  onClose: () => void;
  children: ReactNode;
}

export function Modal({ title, onClose, children }: ModalProps) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  return (
    <div className="modal-backdrop" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="modal" role="dialog" aria-label={title}>
        <div className="modal-head">
          <h3>{title}</h3>
          <button className="mini" onClick={onClose} aria-label="Cerrar">✕</button>
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
        <button onClick={onClose}>Cancelar</button>
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
        <button onClick={onClose}>Cancelar</button>
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
