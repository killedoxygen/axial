import type { RefObject } from 'preact';
import { useEffect } from 'preact/hooks';

/** Closes something that floats when the pointer goes down outside it or Escape is pressed. */
export function useDismiss(ref: RefObject<HTMLElement>, open: boolean, onClose: () => void): void {
  useEffect(() => {
    if (!open) return undefined;
    const onPointer = (event: MouseEvent): void => {
      if (!ref.current?.contains(event.target as Node)) onClose();
    };
    const onKey = (event: KeyboardEvent): void => {
      if (event.key === 'Escape') onClose();
    };
    document.addEventListener('mousedown', onPointer);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onPointer);
      document.removeEventListener('keydown', onKey);
    };
  }, [open]);
}
