import type { JSX } from 'preact';
import { Icon } from './Icons';
import { toasts } from '../store';
import { dismissToast } from '../toast';
import './toast.css';

const ICON_FOR: Record<string, string> = {
  success: 'check',
  error: 'alert',
  info: 'info',
};

export function ToastHost(): JSX.Element | null {
  const list = toasts.value;
  if (list.length === 0) return null;
  return (
    <div class="cp-toasts" role="region" aria-live="polite" aria-label="Notifications">
      {list.map(t => {
        const kind = t.type || 'success';
        return (
          <div key={t.id} class={`cp-toast cp-toast--${kind}`} role="status">
            <span class="cp-toast-icon">
              <Icon name={ICON_FOR[kind] || 'info'} size={14} stroke={2.2} />
            </span>
            <span class="cp-toast-msg">{t.message}</span>
            <button class="cp-toast-close" aria-label="Dismiss" onClick={() => dismissToast(t.id)} data-sound-silent="true">
              <Icon name="x" size={12} stroke={2} />
            </button>
          </div>
        );
      })}
    </div>
  );
}
