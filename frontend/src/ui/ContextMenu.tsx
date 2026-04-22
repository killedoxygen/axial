import type { JSX } from 'preact';
import { signal } from '@preact/signals';
import { useEffect, useRef } from 'preact/hooks';
import { Icon } from './Icons';
import './context-menu.css';

export interface ContextMenuItem {
  icon?: string;
  label: string;
  onSelect: () => void;
  danger?: boolean;
  divider?: boolean;
}

interface MenuSpec {
  x: number;
  y: number;
  items: ContextMenuItem[];
}

const current = signal<MenuSpec | null>(null);

export function openContextMenu(e: MouseEvent, items: ContextMenuItem[]): void {
  e.preventDefault();
  current.value = { x: e.clientX, y: e.clientY, items };
}

export function closeContextMenu(): void { current.value = null; }

export function ContextMenuHost(): JSX.Element | null {
  const spec = current.value;
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!spec) return;
    const onDown = (e: MouseEvent): void => {
      if (ref.current && !ref.current.contains(e.target as Node)) closeContextMenu();
    };
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === 'Escape') closeContextMenu();
    };
    const onScroll = (): void => closeContextMenu();
    // Use capture so we beat child handlers.
    window.addEventListener('mousedown', onDown, true);
    window.addEventListener('keydown', onKey);
    window.addEventListener('scroll', onScroll, true);
    return () => {
      window.removeEventListener('mousedown', onDown, true);
      window.removeEventListener('keydown', onKey);
      window.removeEventListener('scroll', onScroll, true);
    };
  }, [spec]);

  if (!spec) return null;

  // Keep menu inside viewport.
  const max = 240;
  const clampedX = Math.min(spec.x, window.innerWidth - max - 8);
  const clampedY = Math.min(spec.y, window.innerHeight - spec.items.length * 32 - 32);

  return (
    <div
      ref={ref}
      class="cp-ctx-host"
      style={{ left: clampedX, top: clampedY }}
      role="menu"
    >
      {spec.items.map((it, i) => (
        it.divider
          ? <div key={i} class="cp-ctx-divider" />
          : (
            <button
              key={i}
              class={`cp-ctx-item${it.danger ? ' cp-ctx-item--danger' : ''}`}
              role="menuitem"
              onClick={() => { closeContextMenu(); it.onSelect(); }}
            >
              {it.icon && <Icon name={it.icon} size={15} />}
              {it.label}
            </button>
          )
      ))}
    </div>
  );
}
