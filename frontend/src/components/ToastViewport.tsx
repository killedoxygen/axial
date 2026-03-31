import type { JSX } from 'preact';
import { toasts } from '../store';
import { dismissToast } from '../toast';

export function ToastViewport(): JSX.Element {
  const items = toasts.value;

  return (
    <div class="app-toast-viewport" aria-live="polite" aria-atomic="true" role="status">
      {items.map((toast) => (
        <button
          key={toast.id}
          type="button"
          class={`app-toast ${toast.type === 'error' ? 'app-toast-error' : 'app-toast-success'}`}
          onClick={() => dismissToast(toast.id)}
        >
          {toast.message}
        </button>
      ))}
    </div>
  );
}
