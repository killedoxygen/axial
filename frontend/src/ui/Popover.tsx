import type { ComponentChildren, JSX, RefObject } from 'preact';
import { createContext } from 'preact';
import { createPortal } from 'preact/compat';
import { useContext, useEffect, useLayoutEffect, useMemo, useRef } from 'preact/hooks';
import { leaveBehind } from './motion';

const MARGIN = 10;

/** An element, a ref to one, or a bare point such as the cursor. */
export type PopoverAnchor = RefObject<HTMLElement> | HTMLElement | DOMRect;

type Register = (element: HTMLElement) => () => void;

/** Popovers opened from inside another count as inside it, so pressing them keeps the outer one open. */
const NestedPopovers = createContext<Register | null>(null);

function anchorElement(anchor: PopoverAnchor): HTMLElement | null {
  if (anchor instanceof DOMRect) return null;
  return anchor instanceof HTMLElement ? anchor : anchor.current;
}

const clamp = (value: number, size: number, extent: number): number =>
  Math.max(MARGIN, Math.min(value, extent - size - MARGIN));

/** Where along an edge the popover starts: `start` hangs from the anchor's start, unless that leaves the window. */
function along(align: 'start' | 'center' | 'end', start: number, length: number, size: number, extent: number): number {
  const end = start + length - size;
  const at =
    align === 'center' ? start + (length - size) / 2 : align === 'end' || start + size > extent - MARGIN ? end : start;
  return clamp(at, size, extent);
}

/**
 * Floats from its anchor without moving anything around it: on its side of the anchor, or the opposite side where
 * the window is short. Mounted only while open; a press, scroll or resize outside it (or its nested popovers), or
 * Escape, dismisses it. A `passive` popover, like a tooltip, never takes focus or Escape from what it describes.
 */
export function Popover({
  anchor,
  popoverRef,
  onDismiss,
  side = 'below',
  align = 'start',
  gap = 6,
  fit = 'content',
  maxHeight = 320,
  passive = false,
  class: className,
  children,
  ...props
}: Omit<JSX.HTMLAttributes<HTMLDivElement>, 'ref' | 'style' | 'class'> & {
  anchor: PopoverAnchor;
  popoverRef?: { current: HTMLDivElement | null };
  onDismiss: () => void;
  side?: 'below' | 'right';
  align?: 'start' | 'center' | 'end';
  gap?: number;
  /** `anchor`: exactly the anchor's width. `min-anchor`: at least that wide. */
  fit?: 'content' | 'anchor' | 'min-anchor';
  maxHeight?: number;
  passive?: boolean;
  class?: string;
  children: ComponentChildren;
}): JSX.Element {
  const ref = useRef<HTMLDivElement | null>(null);
  const dismiss = useRef(onDismiss);
  dismiss.current = onDismiss;
  const nested = useRef(new Set<HTMLElement>());
  const registerInParent = useContext(NestedPopovers);
  const register = useMemo<Register>(
    () => (element) => {
      nested.current.add(element);
      const unregister = registerInParent?.(element);
      return () => {
        nested.current.delete(element);
        unregister?.();
      };
    },
    [registerInParent],
  );

  useLayoutEffect(() => {
    const element = ref.current;
    const rect = anchor instanceof DOMRect ? anchor : anchorElement(anchor)?.getBoundingClientRect();
    if (!element || !rect) return undefined;
    const { innerWidth, innerHeight } = window;
    if (fit === 'anchor') element.style.width = `${rect.width}px`;
    if (fit === 'min-anchor') element.style.minWidth = `${rect.width}px`;
    if (side === 'below') {
      const below = innerHeight - rect.bottom - gap - MARGIN;
      const above = rect.top - gap - MARGIN;
      const flip = Math.min(element.scrollHeight, maxHeight) > below && above > below;
      element.style.maxHeight = `${Math.min(maxHeight, Math.max(96, flip ? above : below))}px`;
      element.style.left = `${along(align, rect.left, rect.width, element.offsetWidth, innerWidth)}px`;
      // Hung by the edge nearest the anchor, so content that grows or shrinks stays attached to it.
      if (flip) element.style.bottom = `${innerHeight - rect.top + gap}px`;
      else element.style.top = `${rect.bottom + gap}px`;
      element.dataset.side = flip ? 'above' : 'below';
    } else {
      element.style.maxHeight = `${Math.min(maxHeight, innerHeight - 2 * MARGIN)}px`;
      const width = element.offsetWidth;
      const flip = rect.right + gap + width > innerWidth - MARGIN && rect.left - gap - width > MARGIN;
      element.style.left = `${flip ? rect.left - gap - width : rect.right + gap}px`;
      element.style.top = `${along(align, rect.top, rect.height, element.offsetHeight, innerHeight)}px`;
      element.dataset.side = flip ? 'left' : 'right';
    }
    return passive ? undefined : registerInParent?.(element);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // It leaves the way it came, after it is gone.
  useLayoutEffect(() => () => leaveBehind(ref.current), []);

  useEffect(() => {
    const onDismiss = (): void => dismiss.current();
    const inside = (target: EventTarget | null): boolean =>
      target instanceof Node &&
      (ref.current?.contains(target) === true || [...nested.current].some((element) => element.contains(target)));
    const onPointerDown = (event: PointerEvent): void => {
      if (!inside(event.target) && !anchorElement(anchor)?.contains(event.target as Node)) onDismiss();
    };
    const onScroll = (event: Event): void => {
      if (!inside(event.target)) onDismiss();
    };
    // Only the innermost open popover answers Escape, and it keeps the key from closing what holds it.
    const onKeyDown = (event: KeyboardEvent): void => {
      if (event.key !== 'Escape' || nested.current.size > 0) return;
      onDismiss();
      if (passive) return;
      event.preventDefault();
      event.stopPropagation();
      if (inside(document.activeElement)) anchorElement(anchor)?.focus();
    };
    document.addEventListener('pointerdown', onPointerDown, true);
    document.addEventListener('scroll', onScroll, true);
    document.addEventListener('keydown', onKeyDown, true);
    window.addEventListener('resize', onDismiss);
    window.addEventListener('blur', onDismiss);
    return () => {
      document.removeEventListener('pointerdown', onPointerDown, true);
      document.removeEventListener('scroll', onScroll, true);
      document.removeEventListener('keydown', onKeyDown, true);
      window.removeEventListener('resize', onDismiss);
      window.removeEventListener('blur', onDismiss);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Inside a dialog it stays in the dialog, so its focus trap and modality still include it.
  const host = anchorElement(anchor)?.closest<HTMLElement>('[role="dialog"]') ?? document.body;
  return createPortal(
    <NestedPopovers.Provider value={register}>
      <div
        {...props}
        ref={(element) => {
          ref.current = element;
          if (popoverRef) popoverRef.current = element;
        }}
        class={className ? `cp-popover ${className}` : 'cp-popover'}
      >
        {children}
      </div>
    </NestedPopovers.Provider>,
    host,
  );
}
