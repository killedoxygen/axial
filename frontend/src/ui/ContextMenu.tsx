import type { JSX } from 'preact';
import { signal } from '@preact/signals';
import { Icon, type IconName } from './Icons';
import { Popover } from './Popover';

export interface ContextMenuItem {
  icon?: IconName;
  label: string;
  onSelect: () => void;
  danger?: boolean;
  divider?: boolean;
}

interface MenuSpec {
  at: DOMRect;
  items: ContextMenuItem[];
}

const current = signal<MenuSpec | null>(null);

export function openContextMenu(e: MouseEvent, items: ContextMenuItem[]): void {
  e.preventDefault();
  current.value = { at: new DOMRect(e.clientX, e.clientY, 0, 0), items };
}

export function closeContextMenu(): void {
  current.value = null;
}

export function ContextMenuHost(): JSX.Element | null {
  const spec = current.value;
  if (!spec) return null;
  return (
    // Keyed by its point, so opening it elsewhere places it afresh.
    <Popover
      key={`${spec.at.x},${spec.at.y}`}
      anchor={spec.at}
      gap={0}
      onDismiss={closeContextMenu}
      class="cp-ctx-host cp-pop cp-menu"
      role="menu"
    >
      {spec.items.map((it, i) =>
        it.divider ? (
          <div key={i} class="cp-menu-gap" />
        ) : (
          <button
            key={i}
            class={`cp-menu-item${it.danger ? ' cp-menu-item--danger' : ''}`}
            role="menuitem"
            onClick={() => {
              closeContextMenu();
              it.onSelect();
            }}
          >
            {it.icon && <Icon name={it.icon} size={15} />}
            {it.label}
          </button>
        ),
      )}
    </Popover>
  );
}
