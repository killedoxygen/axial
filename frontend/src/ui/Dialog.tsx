import type { JSX } from 'preact';
import { signal } from '@preact/signals';
import { useEffect, useState } from 'preact/hooks';
import { Button, Input } from './Atoms';
import './dialog.css';

type DialogResult = boolean | string | null;

interface PromptOptions {
  title?: string;
  placeholder?: string;
  confirmText?: string;
  destructive?: boolean;
  validate?: (value: string) => string | null;
  normalizeInput?: (value: string) => string;
  normalizeValue?: (value: string) => string;
}

interface DialogSpec {
  kind: 'confirm' | 'prompt' | 'alert';
  title?: string;
  message: string;
  initialValue?: string;
  placeholder?: string;
  confirmText?: string;
  cancelText?: string | null;
  destructive?: boolean;
  validate?: (value: string) => string | null;
  normalizeInput?: (value: string) => string;
  normalizeValue?: (value: string) => string;
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

export function prompt(message: string, initial = '', opts: PromptOptions = {}): Promise<string | null> {
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
      validate: opts.validate,
      normalizeInput: opts.normalizeInput,
      normalizeValue: opts.normalizeValue,
      resolve: (v) => resolve(typeof v === 'string' ? v : null),
    };
  });
}

export function DialogHost(): JSX.Element | null {
  const spec = current.value;
  const [draft, setDraft] = useState('');
  const [touched, setTouched] = useState(false);

  useEffect(() => {
    if (!spec) return;
    if (spec.kind === 'prompt') {
      setDraft(spec.initialValue || '');
      setTouched(false);
    }
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === 'Escape') { resolveAs(false); }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [spec]);

  if (!spec) return null;

  const promptError = spec.kind === 'prompt' ? (spec.validate?.(draft) ?? null) : null;
  const showPromptError = spec.kind === 'prompt' && promptError !== null && (touched || draft.length > 0);

  const resolveAs = (ok: boolean): void => {
    let payload: DialogResult = ok;
    if (spec.kind === 'prompt') {
      if (!ok) {
        payload = null;
      } else {
        if (promptError) {
          setTouched(true);
          return;
        }
        payload = spec.normalizeValue ? spec.normalizeValue(draft) : draft.trim();
      }
    }
    spec.resolve(payload);
    current.value = null;
  };

  return (
    <div class="cp-dialog-overlay" onClick={(e) => { if (e.target === e.currentTarget) resolveAs(false); }}>
      <div class="cp-dialog" role="dialog" aria-modal="true" aria-labelledby={spec.title ? 'cp-dlg-title' : undefined}>
        {spec.title && <h2 id="cp-dlg-title" class="cp-dialog-title">{spec.title}</h2>}
        {spec.message && <p class="cp-dialog-body">{spec.message}</p>}
        {spec.kind === 'prompt' && (
          <>
            <Input
              value={draft}
              onChange={(value) => {
                setTouched(true);
                setDraft(spec.normalizeInput ? spec.normalizeInput(value) : value);
              }}
              placeholder={spec.placeholder}
              autoFocus
              onKeyDown={(e) => { if (e.key === 'Enter') resolveAs(true); }}
            />
            {showPromptError && <div class="cp-dialog-error">{promptError}</div>}
          </>
        )}
        <div class="cp-dialog-actions">
          {spec.cancelText && (
            <Button variant="ghost" onClick={() => resolveAs(false)}>{spec.cancelText}</Button>
          )}
          <Button
            variant={spec.destructive ? 'danger' : 'primary'}
            disabled={spec.kind === 'prompt' && promptError !== null}
            onClick={() => resolveAs(true)}
          >{spec.confirmText}</Button>
        </div>
      </div>
    </div>
  );
}
