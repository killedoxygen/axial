import { toasts } from './store';
import type { ToastItem, ToastKind } from './types';

let nextToastId = 1;
const toastTimers = new Map<number, ReturnType<typeof setTimeout>>();

export function dismissToast(id: number): void {
  const timer = toastTimers.get(id);
  if (timer) {
    clearTimeout(timer);
    toastTimers.delete(id);
  }
  toasts.value = toasts.value.filter((toast) => toast.id !== id);
}

function scheduleToastRemoval(item: ToastItem): void {
  const duration = item.type === 'error' ? 5000 : 3000;
  toastTimers.set(item.id, setTimeout(() => dismissToast(item.id), duration));
}

export function toast(message: string, type: ToastKind = 'success'): void {
  const item: ToastItem = { id: nextToastId++, message, type };
  toasts.value = [...toasts.value, item];
  scheduleToastRemoval(item);
}
