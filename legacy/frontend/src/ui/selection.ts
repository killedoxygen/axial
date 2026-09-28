import { useEffect, useMemo, useState } from 'preact/hooks';
import type { ContextMenuItem } from './ContextMenu';

export interface SelectionState<T> {
  selectedIds: Set<string>;
  selectedItems: T[];
  selectedCount: number;
  allSelected: boolean;
  someSelected: boolean;
  isSelected: (id: string) => boolean;
  select: (id: string) => void;
  deselect: (id: string) => void;
  toggle: (id: string) => void;
  clear: () => void;
  selectAll: () => void;
  selectOnly: (id: string) => void;
}

export function useSelection<T>(items: T[], getId: (item: T) => string): SelectionState<T> {
  const [selectedIds, setSelectedIds] = useState<Set<string>>(() => new Set());
  const itemIds = useMemo(() => items.map(getId), [items, getId]);
  const idSet = useMemo(() => new Set(itemIds), [itemIds]);
  const selectedItems = useMemo(
    () => items.filter((item) => selectedIds.has(getId(item))),
    [items, getId, selectedIds],
  );

  useEffect(() => {
    setSelectedIds((current) => {
      let changed = false;
      const next = new Set<string>();
      current.forEach((id) => {
        if (idSet.has(id)) {
          next.add(id);
        } else {
          changed = true;
        }
      });
      return changed ? next : current;
    });
  }, [idSet]);

  const selectedCount = selectedIds.size;
  const allSelected = itemIds.length > 0 && selectedCount === itemIds.length;
  const someSelected = selectedCount > 0 && !allSelected;

  return {
    selectedIds,
    selectedItems,
    selectedCount,
    allSelected,
    someSelected,
    isSelected: (id) => selectedIds.has(id),
    select: (id) => setSelectedIds((current) => new Set(current).add(id)),
    deselect: (id) =>
      setSelectedIds((current) => {
        const next = new Set(current);
        next.delete(id);
        return next;
      }),
    toggle: (id) =>
      setSelectedIds((current) => {
        const next = new Set(current);
        if (next.has(id)) {
          next.delete(id);
        } else {
          next.add(id);
        }
        return next;
      }),
    clear: () => setSelectedIds(new Set()),
    selectAll: () => setSelectedIds(new Set(itemIds)),
    selectOnly: (id) => setSelectedIds(new Set([id])),
  };
}

export function selectionToggleLabel(selected: boolean, name?: string): string {
  const action = selected ? 'Deselect' : 'Select';
  return name ? `${action} ${name}` : action;
}

export function selectionMenuItem<T>(
  selection: Pick<SelectionState<T>, 'isSelected' | 'toggle'>,
  id: string,
): ContextMenuItem {
  const selected = selection.isSelected(id);
  return {
    icon: selected ? 'x' : 'check',
    label: selected ? 'Deselect' : 'Select',
    onSelect: () => selection.toggle(id),
  };
}
