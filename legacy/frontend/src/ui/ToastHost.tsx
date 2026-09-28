import type { JSX } from 'preact';
import { Icon, type IconName } from './Icons';
import { toasts } from '../store';
import { dismissToast } from '../toast';
import type { ToastKind } from '../types-ui';

const ICON_FOR: Record<ToastKind, IconName> = {
  success: 'check',
  error: 'alert',
  info: 'info',
};

export function ToastHost(): JSX.Element | null {
  const list = toasts.value;
  return (
    <div class="cp-toasts" role="region" aria-live="polite" aria-label="Notifications">
      {list.map((t) => {
        const kind = t.type;
        return (
          <div key={t.id} class={`cp-toast cp-toast--${kind}`} role="status">
            <span class="cp-toast-icon">
              <Icon name={ICON_FOR[kind]} size={14} stroke={2.2} />
            </span>
            <span class="cp-toast-msg">{t.message}</span>
            <button
              class="cp-toast-close"
              aria-label="Dismiss"
              onClick={() => dismissToast(t.id)}
              data-sound-silent="true"
            >
              <Icon name="x" size={12} stroke={2} />
            </button>
          </div>
        );
      })}
    </div>
  );
}
