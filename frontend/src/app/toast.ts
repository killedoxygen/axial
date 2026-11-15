import { signal } from '@preact/signals';
import { appendLog } from './log';

export type ToastKind = 'success' | 'error' | 'info';

export interface ToastAction {
  label: string;
  run: () => void;
  /** The one the notice is really offering; the rest stay quiet. */
  primary?: boolean;
  title?: string;
}

export interface ToastItem {
  id: number;
  message: string;
  type: ToastKind;
  /** A second, quieter line under the message. */
  detail?: string;
  /** What the user can do about it, there and then. */
  actions?: ToastAction[];
}
let nextToastId = 1;
const toastTimers = new Map<number, ReturnType<typeof setTimeout>>();
/** What keeps each notice up: the pointer over it, focus inside it, or both. */
const toastHolds = new Map<number, Set<ToastHold>>();
export const toasts = signal<ToastItem[]>([]);

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

export function showError(msg: string): void {
  appendLog('stderr', `ERROR: ${msg}`);
  toast(msg, 'error');
}

export function errMessage(err: unknown): string {
  if (err instanceof Error && err.message) return err.message;
  if (typeof err === 'string') return err;
  return 'Unknown error';
}
