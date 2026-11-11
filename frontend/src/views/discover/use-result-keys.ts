import type { RefObject } from 'preact';
import { useEffect } from 'preact/hooks';
import { resultInHand, results } from './state';

/** The result in hand: the one last taken while it is still listed, else the first that still has an action. */
export function inHandResult(): string | null {
  const items = results.value;
  const held = resultInHand.value;
  if (held && items.some((item) => item.canonical_id === held)) return held;
  return (items.find((item) => item.install_state !== 'installed') ?? items[0])?.canonical_id ?? null;
}

/**
 * The keyboard runs the list from the page or its search field: the arrows move the hand, Enter runs the row's
 * action, Right opens its details and "/" returns to the search. Other controls keep their own keys.
 */
export function useResultKeys({
  listRef,
  searchRef,
  onOpen,
}: {
  listRef: RefObject<HTMLElement>;
  searchRef: RefObject<HTMLInputElement>;
  onOpen: (canonicalId: string) => void;
}): void {
  useEffect(() => {
    const row = (id: string): HTMLElement | null =>
      listRef.current?.querySelector<HTMLElement>(`[data-result="${CSS.escape(id)}"]`) ?? null;

    const onKeyDown = (event: KeyboardEvent): void => {
      if (event.defaultPrevented || event.metaKey || event.ctrlKey || event.altKey) return;
      if (document.querySelector('[aria-modal="true"]')) return;
      const target = event.target instanceof HTMLElement ? event.target : null;
      const search = searchRef.current;
      const inSearch = target !== null && target === search;
      // Typing anywhere else, an open menu, and the page's own controls are not the list's.
      if (!inSearch && target?.closest('input, textarea, select, [contenteditable], [role="listbox"], .cp-pop')) return;

      if (event.key === '/' && !inSearch) {
        event.preventDefault();
        search?.focus();
        search?.select();
        return;
      }

      const list = listRef.current;
      const held = inHandResult();
      if (!list || !held) return;
      const loose = !target || target === document.body;
      const onRow = target !== null && list.contains(target);

      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
        // Focus left on the rail after arriving still walks the list.
        const outside = !loose && !list.closest('.cp-view-page')?.contains(target);
        if (!loose && !outside && !inSearch && !onRow) return;
        event.preventDefault();
        const ids = results.value.map((item) => item.canonical_id);
        const at = ids.indexOf(held) + (event.key === 'ArrowDown' ? 1 : -1);
        const next = ids[Math.max(0, Math.min(ids.length - 1, at))]!;
        resultInHand.value = next;
        row(next)?.scrollIntoView({ block: 'nearest' });
      } else if (event.key === 'Enter') {
        // A focused button answers Enter itself.
        if (!loose && !inSearch && !(onRow && !target?.closest('button, a'))) return;
        event.preventDefault();
        row(held)?.querySelector<HTMLElement>('.cp-discover-act button:not(:disabled)')?.click();
      } else if (event.key === 'ArrowRight' && (loose || onRow)) {
        event.preventDefault();
        onOpen(held);
      } else if (event.key === 'Escape' && inSearch) {
        search?.blur();
      }
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, []);
}
