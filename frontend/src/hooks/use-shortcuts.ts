import { useEffect } from 'preact/hooks';
import { SHORTCUTS } from '../shortcuts';

export function useShortcuts(): void {
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent): void => {
      const target = e.target as HTMLElement | null;
      if (target?.closest('input, textarea, [contenteditable]')) return;
      for (const def of SHORTCUTS) {
        if (!def.matches(e)) continue;
        if (def.preventsDefault !== false) e.preventDefault();
        def.run();
        return;
      }
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, []);
}
