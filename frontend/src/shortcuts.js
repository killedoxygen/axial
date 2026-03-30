import { dom, local, saveLocalState } from './state.js';
import { Sound } from './sound.js';
import { esc } from './utils.js';

export const SHORTCUT_DEFAULTS = {
  settings:    { key: ',', ctrl: true, desc: 'Open or close settings' },
  search:      { key: 'f', ctrl: true, desc: 'Focus instance search' },
  newInstance: { key: 'n', ctrl: true, desc: 'New instance' },
  launch:      { key: 'Enter', ctrl: true, desc: 'Launch selected instance' },
  save:        { key: 's', ctrl: true, desc: 'Save settings' },
  close:       { key: 'Escape', ctrl: false, desc: 'Close dialogs' },
};

export const Shortcuts = {
  _custom: {},
  load(stored) { this._custom = stored || {}; },
  get(action) { return this._custom[action] || SHORTCUT_DEFAULTS[action]; },
  set(action, binding) { this._custom[action] = binding; },
  reset(action) { delete this._custom[action]; },
  all() { return Object.keys(SHORTCUT_DEFAULTS); },
  matches(e, action) {
    const b = this.get(action);
    if (!b) return false;
    const key = b.key.length === 1 ? b.key.toLowerCase() : b.key;
    const eKey = e.key.length === 1 ? e.key.toLowerCase() : e.key;
    return eKey === key && !!e.ctrlKey === !!b.ctrl && !!e.shiftKey === !!b.shift && !!e.altKey === !!b.alt;
  },
  format(action) {
    const b = this.get(action);
    if (!b) return '';
    const parts = [];
    if (b.ctrl) parts.push('Ctrl');
    if (b.shift) parts.push('Shift');
    if (b.alt) parts.push('Alt');
    const k = b.key === ' ' ? 'Space' : b.key === ',' ? ',' : b.key.length === 1 ? b.key.toUpperCase() : b.key;
    parts.push(k);
    return parts.join('+');
  },
};

export function syncShortcutHints() {
  document.querySelectorAll('[data-action]').forEach(el => {
    const action = el.dataset.action;
    const label = Shortcuts.format(action);
    if (label) el.setAttribute('data-shortcut-hint', label);
    else el.removeAttribute('data-shortcut-hint');
  });
}

export function renderShortcutEditor() {
  if (!dom.shortcutList) return;
  const labels = { settings: 'Settings', search: 'Search', newInstance: 'New Instance', launch: 'Launch', save: 'Save', close: 'Close' };
  dom.shortcutList.innerHTML = Shortcuts.all().map(action => {
    const b = Shortcuts.get(action);
    const isCustom = !!local.shortcuts[action];
    return `<div class="shortcut-item" data-sc-action="${action}">
      <span class="shortcut-key shortcut-item-key" data-sc-record="${action}" title="Click to change">${esc(Shortcuts.format(action))}</span>
      <span class="shortcut-desc">${esc(b.desc)}${isCustom ? ` <button class="shortcut-item-reset" data-sc-reset="${action}">reset</button>` : ''}</span>
    </div>`;
  }).join('');
}

let recordingAction = null;

export function startRecording(action) {
  stopRecording();
  recordingAction = action;
  const el = dom.shortcutList?.querySelector(`[data-sc-record="${action}"]`);
  if (el) { el.classList.add('recording'); el.textContent = 'Press keys...'; }
}

export function stopRecording() {
  if (!recordingAction) return;
  const el = dom.shortcutList?.querySelector(`[data-sc-record="${recordingAction}"]`);
  if (el) { el.classList.remove('recording'); el.textContent = Shortcuts.format(recordingAction); }
  recordingAction = null;
}

export function handleRecordKey(e) {
  if (!recordingAction) return false;
  e.preventDefault(); e.stopPropagation();
  if (e.key === 'Escape') { stopRecording(); return true; }
  if (['Control', 'Shift', 'Alt', 'Meta'].includes(e.key)) return true;
  Shortcuts.set(recordingAction, { key: e.key, ctrl: e.ctrlKey, shift: e.shiftKey, alt: e.altKey, desc: Shortcuts.get(recordingAction).desc });
  local.shortcuts = Shortcuts._custom;
  saveLocalState();
  stopRecording();
  renderShortcutEditor();
  syncShortcutHints();
  Sound.ui('affirm');
  return true;
}
