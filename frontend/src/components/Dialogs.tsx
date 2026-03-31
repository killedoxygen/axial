import { render } from 'preact';
import type { JSX } from 'preact';
import { useRef, useEffect } from 'preact/hooks';
import { useSignal } from '@preact/signals';
import { Sound } from '../sound';

// ── Types ──

interface ConfirmOptions {
  confirmText?: string;
  cancelText?: string;
  destructive?: boolean;
}

interface PromptOptions {
  confirmText?: string;
  cancelText?: string;
  validate?: (val: string) => string | null;
}

// ── Confirm dialog component ──

function ConfirmDialog({ message, options, onResult }: {
  message: string;
  options: Required<Pick<ConfirmOptions, 'confirmText' | 'cancelText'>> & { destructive: boolean };
  onResult: (result: boolean) => void;
}): JSX.Element {
  const confirmRef = useRef<HTMLButtonElement>(null);

  useEffect(() => { confirmRef.current?.focus(); }, []);

  return (
    <div
      class="modal-overlay"
      id="dialog-overlay"
      onClick={(e) => { if (e.target === e.currentTarget) onResult(false); }}
    >
      <div class="modal" style="width:380px">
        <div style="padding:20px 18px 8px">
          <p style="margin:0;font-family:var(--font-sans);font-size:13px;color:var(--text);line-height:1.5;white-space:pre-line">
            {message}
          </p>
        </div>
        <div style="display:flex;justify-content:flex-end;gap:8px;padding:12px 18px 16px">
          <button class="btn-secondary" onClick={() => onResult(false)}>
            {options.cancelText}
          </button>
          <button
            class={options.destructive ? 'btn-danger' : 'btn-primary'}
            ref={confirmRef}
            onClick={() => onResult(true)}
          >
            {options.confirmText}
          </button>
        </div>
      </div>
    </div>
  );
}

// ── Prompt dialog component ──

function PromptDialog({ message, defaultValue, options, onResult }: {
  message: string;
  defaultValue: string;
  options: Required<Pick<PromptOptions, 'confirmText' | 'cancelText'>> & { validate?: (val: string) => string | null };
  onResult: (result: string | null) => void;
}): JSX.Element {
  const inputRef = useRef<HTMLInputElement>(null);
  const inputValue = useSignal(defaultValue);
  const error = useSignal<string | null>(null);

  useEffect(() => {
    const el = inputRef.current;
    if (el) { el.focus(); el.select(); }
  }, []);

  function tryConfirm(): void {
    const val = inputValue.value.trim();
    if (!val) { inputRef.current?.focus(); return; }
    if (options.validate) {
      const err = options.validate(val);
      if (err) {
        error.value = err;
        inputRef.current?.focus();
        return;
      }
    }
    onResult(val);
  }

  return (
    <div
      class="modal-overlay"
      id="dialog-overlay"
      onClick={(e) => { if (e.target === e.currentTarget) onResult(null); }}
    >
      <div class="modal" style="width:380px">
        <div style="padding:20px 18px 8px;display:flex;flex-direction:column;gap:10px">
          <p style="margin:0;font-family:var(--font-sans);font-size:13px;color:var(--text);line-height:1.5">
            {message}
          </p>
          <input
            type="text"
            class="field-input"
            ref={inputRef}
            value={inputValue.value}
            spellcheck={false}
            autocomplete="off"
            style="width:100%;box-sizing:border-box"
            onInput={(e) => {
              inputValue.value = (e.target as HTMLInputElement).value;
              error.value = null;
            }}
            onKeyDown={(e) => { if (e.key === 'Enter') tryConfirm(); }}
          />
          {error.value && (
            <div style="font-size:11px;color:var(--red)">{error.value}</div>
          )}
        </div>
        <div style="display:flex;justify-content:flex-end;gap:8px;padding:12px 18px 16px">
          <button class="btn-secondary" onClick={() => onResult(null)}>
            {options.cancelText}
          </button>
          <button class="btn-primary" onClick={() => tryConfirm()}>
            {options.confirmText}
          </button>
        </div>
      </div>
    </div>
  );
}

// ── Imperative API ──

function mountDialog(container: HTMLDivElement, node: JSX.Element): void {
  document.body.appendChild(container);
  render(node, container);
  Sound.ui('soft');
}

function unmountDialog(container: HTMLDivElement): void {
  render(null, container);
  container.remove();
}

let dismissActiveDialog: (() => void) | null = null;

export function dismissDialog(): void {
  dismissActiveDialog?.();
}

export function showConfirm(message: string, options: ConfirmOptions = {}): Promise<boolean> {
  const { confirmText = 'Confirm', cancelText = 'Cancel', destructive = false } = options;
  return new Promise(resolve => {
    const container = document.createElement('div');
    dismissActiveDialog?.();
    const close = (result: boolean): void => {
      if (dismissActiveDialog === cancel) dismissActiveDialog = null;
      unmountDialog(container);
      resolve(result);
    };
    const cancel = (): void => close(false);
    dismissActiveDialog = cancel;
    mountDialog(container, (
      <ConfirmDialog
        message={message}
        options={{ confirmText, cancelText, destructive }}
        onResult={close}
      />
    ));
  });
}

export function showPrompt(message: string, defaultValue = '', options: PromptOptions = {}): Promise<string | null> {
  const { confirmText = 'OK', cancelText = 'Cancel', validate } = options;
  return new Promise(resolve => {
    const container = document.createElement('div');
    dismissActiveDialog?.();
    const close = (result: string | null): void => {
      if (dismissActiveDialog === cancel) dismissActiveDialog = null;
      unmountDialog(container);
      resolve(result);
    };
    const cancel = (): void => close(null);
    dismissActiveDialog = cancel;
    mountDialog(container, (
      <PromptDialog
        message={message}
        defaultValue={defaultValue}
        options={{ confirmText, cancelText, validate }}
        onResult={close}
      />
    ));
  });
}
