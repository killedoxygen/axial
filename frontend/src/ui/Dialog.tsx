import type { JSX } from 'preact';
import { signal } from '@preact/signals';
import { useEffect, useState } from 'preact/hooks';
import { Button, Input } from './Atoms';
import './dialog.css';

type DialogResult = boolean | string | null;

interface DialogSpec {
  kind: 'confirm' | 'prompt' | 'alert';
  title?: string;
  message: string;
  initialValue?: string;
  placeholder?: string;
  confirmText?: string;
  cancelText?: string | null;
  destructive?: boolean;
  resolve: (v: DialogResult) => void;
}

const current = signal<DialogSpec | null>(null);

export function showConfirm(message: string, opts: { title?: string; confirmText?: string; cancelText?: string | null; destructive?: boolean } = {}): Promise<boolean> {
  return new Promise(resolve => {
    current.value = {
      kind: 'confirm',
      title: opts.title,
      message,
      confirmText: opts.confirmText || 'Confirm',
      cancelText: opts.cancelText === null ? null : (opts.cancelText || 'Cancel'),
      destructive: opts.destructive,
      resolve: (v) => resolve(v === true),
    };
  });
}

export function showAlert(message: string, title?: string): Promise<void> {
  return new Promise(resolve => {
    current.value = {
      kind: 'alert',
      title,
      message,
      confirmText: 'OK',
      cancelText: null,
      resolve: () => resolve(),
    };
  });
}

export function prompt(message: string, initial = '', opts: { title?: string; placeholder?: string; confirmText?: string; destructive?: boolean } = {}): Promise<string | null> {
  return new Promise(resolve => {
    current.value = {
      kind: 'prompt',
      title: opts.title || message,
      message: opts.title ? message : '',
      initialValue: initial,
      placeholder: opts.placeholder,
      confirmText: opts.confirmText || 'Confirm',
      cancelText: 'Cancel',
      destructive: opts.destructive,
      resolve: (v) => resolve(typeof v === 'string' ? v : null),
    };
  });
}

function PromptInput({ initialValue, placeholder, onSubmit }: { initialValue?: string; placeholder?: string; onSubmit: (v: string) => void }): JSX.Element {
  const [value, setValue] = useState(initialValue || '');
  return (
    <Input
      value={value}
      onChange={setValue}
      placeholder={placeholder}
      autoFocus
      onKeyDown={(e) => { if (e.key === 'Enter') onSubmit(value); }}
    />
  );
}

export function DialogHost(): JSX.Element | null {
  const spec = current.value;
  const [draft, setDraft] = useState('');

  useEffect(() => {
    if (!spec) return;
    if (spec.kind === 'prompt') setDraft(spec.initialValue || '');
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === 'Escape') { resolveAs(false); }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [spec]);

  if (!spec) return null;

  const resolveAs = (ok: boolean): void => {
    const payload: DialogResult = spec.kind === 'prompt' ? (ok ? draft.trim() : null) : ok;
    spec.resolve(payload);
    current.value = null;
  };

  return (
    <div class="cp-dialog-overlay" onClick={(e) => { if (e.target === e.currentTarget) resolveAs(false); }}>
      <div class="cp-dialog" role="dialog" aria-modal="true" aria-labelledby={spec.title ? 'cp-dlg-title' : undefined}>
        {spec.title && <h2 id="cp-dlg-title" class="cp-dialog-title">{spec.title}</h2>}
        {spec.message && <p class="cp-dialog-body">{spec.message}</p>}
        {spec.kind === 'prompt' && (
          <Input
            value={draft}
            onChange={setDraft}
            placeholder={spec.placeholder}
            autoFocus
            onKeyDown={(e) => { if (e.key === 'Enter') resolveAs(true); }}
          />
        )}
        <div class="cp-dialog-actions">
          {spec.cancelText && (
            <Button variant="ghost" onClick={() => resolveAs(false)}>{spec.cancelText}</Button>
          )}
          <Button
            variant={spec.destructive ? 'danger' : 'primary'}
            onClick={() => resolveAs(true)}
          >{spec.confirmText}</Button>
        </div>
      </div>
    </div>
  );
}
