import { signal } from '@preact/signals';
import type { ComponentChildren, JSX } from 'preact';
import { Popover } from './Popover';

let shown = 0;
const tip = signal<{ id: number; content: ComponentChildren; target: HTMLElement; side: 'below' | 'right' } | null>(
  null,
);

/** A tooltip waits for the pointer to rest; once one has shown, the next ones answer at once. */
const REST_MS = 280;
const WARM_MS = 400;
let waiting: number | undefined;
let hiddenAt = -Infinity;

export function hideTip(): void {
  window.clearTimeout(waiting);
  if (tip.value) hiddenAt = performance.now();
  tip.value = null;
}

/** Handlers that name an element in a tooltip while it is pointed at or focused. */
export function tipProps(
  content: ComponentChildren,
  side: 'below' | 'right' = 'below',
): {
  onMouseEnter: (event: JSX.TargetedEvent<HTMLElement>) => void;
  onMouseLeave: () => void;
  onFocus: (event: JSX.TargetedEvent<HTMLElement>) => void;
  onBlur: () => void;
} {
  const show = (target: HTMLElement): void => {
    if (!target.isConnected) return;
    shown += 1;
    tip.value = { id: shown, content, target, side };
  };
  return {
    onMouseEnter: (event) => {
      const target = event.currentTarget;
      window.clearTimeout(waiting);
      if (tip.value || performance.now() - hiddenAt < WARM_MS) show(target);
      else waiting = window.setTimeout(() => show(target), REST_MS);
    },
    onMouseLeave: hideTip,
    onFocus: (event) => {
      window.clearTimeout(waiting);
      show(event.currentTarget);
    },
    onBlur: hideTip,
  };
}

/** The one tooltip, centred on its target's side. */
export function TipHost(): JSX.Element | null {
  const current = tip.value;
  if (!current) return null;
  return (
    <Popover
      key={current.id}
      anchor={current.target}
      side={current.side}
      align="center"
      passive
      onDismiss={hideTip}
      class="cp-tip cp-tip--float"
      role="tooltip"
    >
      {current.content}
    </Popover>
  );
}
