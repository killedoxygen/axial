import { toasts } from './store';
import type { ToastAction, ToastItem, ToastKind } from './types-ui';

let nextToastId = 1;
const toastTimers = new Map<number, ReturnType<typeof setTimeout>>();
/** What keeps each notice up: the pointer over it, focus inside it, or both. */
const toastHolds = new Map<number, Set<ToastHold>>();
export type ToastHold = 'pointer' | 'focus';

export function dismissToast(id: number): void {
  const timer = toastTimers.get(id);
  if (timer) {
    clearTimeout(timer);
    toastTimers.delete(id);
  }
  toastHolds.delete(id);
  toasts.value = toasts.value.filter((toast) => toast.id !== id);
}

function scheduleToastRemoval(item: ToastItem, duration = item.type === 'error' ? 5000 : 3000): void {
  toastTimers.set(
    item.id,
    setTimeout(() => dismissToast(item.id), duration),
  );
}

/** A notice being read or reached for stays; it leaves a moment after neither the pointer nor focus is on it. */
export function holdToast(id: number, by: ToastHold, held: boolean): void {
  if (!toasts.value.some((toast) => toast.id === id)) return;
  const holds = toastHolds.get(id) ?? new Set<ToastHold>();
  if (held) holds.add(by);
  else holds.delete(by);
  toastHolds.set(id, holds);
  const timer = toastTimers.get(id);
  if (timer) clearTimeout(timer);
  toastTimers.delete(id);
  if (holds.size === 0)
    toastTimers.set(
      id,
      setTimeout(() => dismissToast(id), 3000),
    );
}

/** `duration` (ms) keeps something worth reading or acting on up for longer than a passing notice. */
export function toast(
  message: string,
  type: ToastKind = 'success',
  options: { detail?: string; duration?: number; actions?: ToastAction[] } = {},
): void {
  const item: ToastItem = { id: nextToastId++, message, type, detail: options.detail, actions: options.actions };
  toasts.value = [...toasts.value, item];
  scheduleToastRemoval(item, options.duration);
}
