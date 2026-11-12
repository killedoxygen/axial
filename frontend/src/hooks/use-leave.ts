import { useCallback, useRef } from 'preact/hooks';
import { leaveBehind } from '../ui/motion';

/**
 * A `ref` for a surface that should animate out when it unmounts, however that happens. `leave` is how: a surface
 * leaves in place, a page leaves over the view.
 */
export function useLeave<T extends HTMLElement>(
  leave: (element: T | null) => void = leaveBehind,
): (element: T | null) => void {
  const held = useRef<T | null>(null);
  return useCallback((element: T | null) => {
    if (!element) leave(held.current);
    held.current = element;
  }, []);
}
