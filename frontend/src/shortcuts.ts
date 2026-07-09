import { signal } from '@preact/signals';
import { commandPaletteOpen, navigate, openCreate, route } from './ui-state';
import { instances, launchState, runningSessions, selectedInstance } from './store';
import { selectInstance } from './actions';
import { launchGame } from './launch';
import { Sound } from './sound';
import { local, saveLocalState } from './state';
import type { ShortcutBinding } from './types-ui';

export type ShortcutId = 'open-settings' | 'new-instance' | 'command-palette' | 'launch-selected' | 'dismiss';

export type ShortcutDef = {
  id: ShortcutId;
  label: string;
  defaultCombos: ShortcutBinding[];
  run: () => void;
  fixed?: boolean;
  when?: () => boolean;
  preventsDefault?: boolean;
};

export const shortcutsVersion = signal(0);

export const SHORTCUTS: ShortcutDef[] = [
  {
    id: 'open-settings',
    label: 'Open settings',
    defaultCombos: [{ key: ',', ctrl: true }],
    run: () => {
      navigate({ name: 'settings' });
      Sound.ui('theme');
    },
  },
  {
    id: 'new-instance',
    label: 'New instance',
    defaultCombos: [{ key: 'n', ctrl: true }],
    run: () => {
      openCreate();
      Sound.ui('soft');
    },
  },
  {
    id: 'command-palette',
    label: 'Command palette',
    defaultCombos: [
      { key: 'k', ctrl: true },
      { key: 'f', ctrl: true },
    ],
    run: () => {
      commandPaletteOpen.value = true;
      Sound.ui('soft');
    },
  },
  {
    id: 'launch-selected',
    label: 'Launch selected instance',
    defaultCombos: [{ key: 'Enter', ctrl: true }],
    run: () => {
      const currentRoute = route.value;
      let inst = selectedInstance.value;
      if (!inst && currentRoute.name === 'instance') {
        inst = instances.value.find((i) => i.id === currentRoute.id) ?? null;
        if (inst) selectInstance(inst.id);
      }
      if (!inst) return;
      if (runningSessions.value[inst.id]) return;
      if (launchState.value.status === 'preparing') return;
      Sound.ui('launchPress');
      void launchGame();
    },
  },
  {
    id: 'dismiss',
    label: 'Close dialogs',
    defaultCombos: [{ key: 'Escape' }],
    fixed: true,
    preventsDefault: false,
    when: () => commandPaletteOpen.value,
    run: () => {
      commandPaletteOpen.value = false;
    },
  },
];

export function shortcutById(id: ShortcutId): ShortcutDef {
  return SHORTCUTS.find((def) => def.id === id)!;
}

export function shortcutOverride(id: ShortcutId): ShortcutBinding | null {
  return local.shortcuts[id] ?? null;
}

export function setShortcutOverride(id: ShortcutId, combo: ShortcutBinding | null): void {
  const overrides = { ...local.shortcuts };
  if (combo) overrides[id] = combo;
  else delete overrides[id];
  local.shortcuts = overrides;
  saveLocalState();
  shortcutsVersion.value += 1;
}

export function effectiveCombos(def: ShortcutDef): ShortcutBinding[] {
  if (def.fixed) return def.defaultCombos;
  const override = shortcutOverride(def.id);
  return override ? [override] : def.defaultCombos;
}

function normalizedKey(key: string): string {
  return key.length === 1 ? key.toLowerCase() : key;
}

export function comboMatches(combo: ShortcutBinding, e: KeyboardEvent): boolean {
  return (
    normalizedKey(combo.key) === normalizedKey(e.key) &&
    !!combo.ctrl === e.ctrlKey &&
    !!combo.shift === e.shiftKey &&
    !!combo.alt === e.altKey &&
    !!combo.meta === e.metaKey
  );
}

export function shortcutMatches(def: ShortcutDef, e: KeyboardEvent): boolean {
  if (def.when && !def.when()) return false;
  return effectiveCombos(def).some((combo) => comboMatches(combo, e));
}

function sameCombo(a: ShortcutBinding, b: ShortcutBinding): boolean {
  return (
    normalizedKey(a.key) === normalizedKey(b.key) &&
    !!a.ctrl === !!b.ctrl &&
    !!a.shift === !!b.shift &&
    !!a.alt === !!b.alt &&
    !!a.meta === !!b.meta
  );
}

export function findConflict(id: ShortcutId, combo: ShortcutBinding): ShortcutDef | null {
  return (
    SHORTCUTS.find((def) => def.id !== id && effectiveCombos(def).some((existing) => sameCombo(existing, combo))) ??
    null
  );
}

export function captureCombo(e: KeyboardEvent): ShortcutBinding | null {
  if (e.key === 'Shift' || e.key === 'Control' || e.key === 'Alt' || e.key === 'Meta') return null;
  if (!e.ctrlKey && !e.altKey && !e.metaKey && !/^F\d{1,2}$/.test(e.key)) return null;
  const combo: ShortcutBinding = { key: e.key };
  if (e.ctrlKey) combo.ctrl = true;
  if (e.shiftKey) combo.shift = true;
  if (e.altKey) combo.alt = true;
  if (e.metaKey) combo.meta = true;
  return combo;
}

function displayKey(key: string): string {
  if (key === 'Escape') return 'Esc';
  if (key === ' ') return 'Space';
  return key.length === 1 ? key.toUpperCase() : key;
}

export function comboParts(combo: ShortcutBinding): string[] {
  const parts: string[] = [];
  if (combo.ctrl) parts.push('Ctrl');
  if (combo.alt) parts.push('Alt');
  if (combo.shift) parts.push('Shift');
  if (combo.meta) parts.push('Cmd');
  parts.push(displayKey(combo.key));
  return parts;
}

export function shortcutHint(id: ShortcutId, separator = ' '): string {
  return comboParts(effectiveCombos(shortcutById(id))[0]!).join(separator);
}
