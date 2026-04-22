import { useEffect } from 'preact/hooks';
import { navigate, route, commandPaletteOpen } from '../ui-state';
import { selectedInstance, runningSessions, launchState, instances } from '../store';
import { selectInstance } from '../actions';
import { launchGame } from '../launch';
import { Sound } from '../sound';

function match(e: KeyboardEvent, key: string, ctrl = true): boolean {
  const k = key.length === 1 ? key.toLowerCase() : key;
  const ek = e.key.length === 1 ? e.key.toLowerCase() : e.key;
  return ek === k && !!e.ctrlKey === ctrl && !e.shiftKey && !e.altKey && !e.metaKey;
}

// Global keyboard shortcuts, wired to the ui-state signals
export function useShortcuts(): void {
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent): void => {
      // Don't steal typing shortcuts from fields except the global ones
      const target = e.target as HTMLElement | null;
      const typing = !!target?.closest('input, textarea, [contenteditable]');

      if (match(e, ',')) {
        e.preventDefault();
        navigate({ name: 'settings' });
        Sound.ui('theme');
        return;
      }
      if (match(e, 'n')) {
        e.preventDefault();
        navigate({ name: 'create' });
        Sound.ui('soft');
        return;
      }
      if (match(e, 'f')) {
        if (typing) return;
        e.preventDefault();
        commandPaletteOpen.value = true;
        Sound.ui('soft');
        return;
      }
      if (match(e, 'k')) {
        e.preventDefault();
        commandPaletteOpen.value = true;
        Sound.ui('soft');
        return;
      }
      if (match(e, 'Enter')) {
        if (typing) return;
        e.preventDefault();
        const currentRoute = route.value;
        let inst = selectedInstance.value;
        if (!inst && currentRoute.name === 'instance') {
          inst = instances.value.find(i => i.id === currentRoute.id) ?? null;
          if (inst) selectInstance(inst.id);
        }
        if (!inst) return;
        if (runningSessions.value[inst.id]) return;
        if (launchState.value.status === 'preparing') return;
        Sound.ui('launchPress');
        void launchGame();
        return;
      }
      if (e.key === 'Escape' && !typing) {
        if (commandPaletteOpen.value) { commandPaletteOpen.value = false; return; }
      }
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, []);
}
