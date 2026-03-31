import { signal } from '@preact/signals';
import { local, saveLocalState } from './state';
import { Sound } from './sound';
import type { ShortcutBinding } from './types';

export const SHORTCUT_DEFAULTS: Record<string, ShortcutBinding> = {
  settings:    { key: ',', ctrl: true, desc: 'Open or close settings' },
  search:      { key: 'f', ctrl: true, desc: 'Focus instance search' },
  newInstance: { key: 'n', ctrl: true, desc: 'New instance' },
  launch:      { key: 'Enter', ctrl: true, desc: 'Launch selected instance' },
  save:        { key: 's', ctrl: true, desc: 'Save settings' },
  close:       { key: 'Escape', ctrl: false, desc: 'Close dialogs' },
};

export const recordingShortcut = signal<string | null>(null);

export const Shortcuts = {
  _custom: {} as Record<string, ShortcutBinding>,
  load(stored: Record<string, ShortcutBinding> | null): void { this._custom = stored || {}; },
  get(action: string): ShortcutBinding | undefined { return this._custom[action] || SHORTCUT_DEFAULTS[action]; },
  set(action: string, binding: ShortcutBinding): void { this._custom[action] = binding; },
  reset(action: string): void { delete this._custom[action]; },
  all(): string[] { return Object.keys(SHORTCUT_DEFAULTS); },
  matches(e: KeyboardEvent, action: string): boolean {
    const b = this.get(action);
    if (!b) return false;
    const key = b.key.length === 1 ? b.key.toLowerCase() : b.key;
    const eKey = e.key.length === 1 ? e.key.toLowerCase() : e.key;
    return eKey === key && !!e.ctrlKey === !!b.ctrl && !!e.shiftKey === !!b.shift && !!e.altKey === !!b.alt;
  },
  format(action: string): string {
    const b = this.get(action);
    if (!b) return '';
    const parts: string[] = [];
    if (b.ctrl) parts.push('Ctrl');
    if (b.shift) parts.push('Shift');
    if (b.alt) parts.push('Alt');
    const k = b.key === ' ' ? 'Space' : b.key === ',' ? ',' : b.key.length === 1 ? b.key.toUpperCase() : b.key;
    parts.push(k);
    return parts.join('+');
  },
};

export function syncShortcutHints(): void {
  document.querySelectorAll('[data-action]').forEach(el => {
    const action = (el as HTMLElement).dataset.action;
    const label = Shortcuts.format(action!);
    if (label) el.setAttribute('data-shortcut-hint', label);
    else el.removeAttribute('data-shortcut-hint');
  });
}

export function startRecording(action: string): void {
  recordingShortcut.value = action;
}

export function stopRecording(): void {
  recordingShortcut.value = null;
}

export function resetShortcut(action: string): void {
  Shortcuts.reset(action);
  local.shortcuts = { ...Shortcuts._custom };
  saveLocalState();
  syncShortcutHints();
  stopRecording();
  Sound.ui('soft');
}

export function handleRecordKey(e: KeyboardEvent): boolean {
  const action = recordingShortcut.value;
  if (!action) return false;
  e.preventDefault(); e.stopPropagation();
  if (e.key === 'Escape') { stopRecording(); return true; }
  if (['Control', 'Shift', 'Alt', 'Meta'].includes(e.key)) return true;
  Shortcuts.set(action, { key: e.key, ctrl: e.ctrlKey, shift: e.shiftKey, alt: e.altKey, desc: Shortcuts.get(action)!.desc });
  local.shortcuts = { ...Shortcuts._custom };
  saveLocalState();
  stopRecording();
  syncShortcutHints();
  Sound.ui('affirm');
  return true;
}
