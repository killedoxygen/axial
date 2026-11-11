import { signal } from '@preact/signals';
import type { ComponentChildren, JSX } from 'preact';

const tip = signal<{ content: ComponentChildren; rect: DOMRect } | null>(null);
const hide = (): void => {
  tip.value = null;
};

/** Handlers that name an element in a tooltip while it is pointed at or focused. */
export function tipProps(content: ComponentChildren): {
  onMouseEnter: (event: JSX.TargetedEvent<HTMLElement>) => void;
  onMouseLeave: () => void;
  onFocus: (event: JSX.TargetedEvent<HTMLElement>) => void;
  onBlur: () => void;
} {
  const show = (event: JSX.TargetedEvent<HTMLElement>): void => {
    tip.value = { content, rect: event.currentTarget.getBoundingClientRect() };
  };
  return { onMouseEnter: show, onMouseLeave: hide, onFocus: show, onBlur: hide };
}

/** The one tooltip: under its target, or above it when there is no room below. */
export function TipHost(): JSX.Element | null {
  const shown = tip.value;
  if (!shown) return null;
  const { rect } = shown;
  const below = rect.bottom + 40 < window.innerHeight;
  return (
    <span
      class="cp-tip cp-tip--float"
      role="tooltip"
      data-placement={below ? 'below' : 'above'}
      style={{ left: `${rect.left + rect.width / 2}px`, top: `${below ? rect.bottom + 6 : rect.top - 6}px` }}
    >
      {shown.content}
    </span>
  );
}
