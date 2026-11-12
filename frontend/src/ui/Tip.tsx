import { signal } from '@preact/signals';
import type { ComponentChildren, JSX } from 'preact';
import { Popover } from './Popover';

let shown = 0;
const tip = signal<{ id: number; content: ComponentChildren; target: HTMLElement; side: 'below' | 'right' } | null>(
  null,
);

export function hideTip(): void {
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
  const show = (event: JSX.TargetedEvent<HTMLElement>): void => {
    shown += 1;
    tip.value = { id: shown, content, target: event.currentTarget, side };
  };
  return { onMouseEnter: show, onMouseLeave: hideTip, onFocus: show, onBlur: hideTip };
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
