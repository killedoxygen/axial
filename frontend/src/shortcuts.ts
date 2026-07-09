import { commandPaletteOpen, navigate, openCreate, route } from './ui-state';
import { instances, launchState, runningSessions, selectedInstance } from './store';
import { selectInstance } from './actions';
import { launchGame } from './launch';
import { Sound } from './sound';

export type ShortcutId = 'open-settings' | 'new-instance' | 'command-palette' | 'launch-selected' | 'dismiss';

export type ShortcutDef = {
  id: ShortcutId;
  label: string;
  combos: string[][];
  matches: (e: KeyboardEvent) => boolean;
  run: () => void;
  preventsDefault?: boolean;
};

function ctrlCombo(e: KeyboardEvent, key: string): boolean {
  const k = key.length === 1 ? key.toLowerCase() : key;
  const ek = e.key.length === 1 ? e.key.toLowerCase() : e.key;
  return ek === k && e.ctrlKey && !e.shiftKey && !e.altKey && !e.metaKey;
}

export const SHORTCUTS: ShortcutDef[] = [
  {
    id: 'open-settings',
    label: 'Open settings',
    combos: [['Ctrl', ',']],
    matches: (e) => ctrlCombo(e, ','),
    run: () => {
      navigate({ name: 'settings' });
      Sound.ui('theme');
    },
  },
  {
    id: 'new-instance',
    label: 'New instance',
    combos: [['Ctrl', 'N']],
    matches: (e) => ctrlCombo(e, 'n'),
    run: () => {
      openCreate();
      Sound.ui('soft');
    },
  },
  {
    id: 'command-palette',
    label: 'Command palette',
    combos: [
      ['Ctrl', 'K'],
      ['Ctrl', 'F'],
    ],
    matches: (e) => ctrlCombo(e, 'k') || ctrlCombo(e, 'f'),
    run: () => {
      commandPaletteOpen.value = true;
      Sound.ui('soft');
    },
  },
  {
    id: 'launch-selected',
    label: 'Launch selected instance',
    combos: [['Ctrl', 'Enter']],
    matches: (e) => ctrlCombo(e, 'Enter'),
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
    combos: [['Esc']],
    preventsDefault: false,
    matches: (e) => e.key === 'Escape' && commandPaletteOpen.value,
    run: () => {
      commandPaletteOpen.value = false;
    },
  },
];

export function shortcutById(id: ShortcutId): ShortcutDef {
  return SHORTCUTS.find((def) => def.id === id)!;
}

export function shortcutHint(id: ShortcutId, separator = ' '): string {
  return shortcutById(id).combos[0]!.join(separator);
}
