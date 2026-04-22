import type { JSX } from 'preact';
import { Icon } from './Icons';
import { toasts } from '../store';
import { dismissToast } from '../toast';
import './toast.css';

export function ToastHost(): JSX.Element | null {
  const list = toasts.value;
  if (list.length === 0) return null;
  return (
    <div class="cp-toasts">
      {list.map(t => (
        <div key={t.id} class={`cp-toast cp-toast--${t.type || 'success'}`}>
          <span>{t.message}</span>
          <button class="cp-toast-close" aria-label="Dismiss" onClick={() => dismissToast(t.id)}>
            <Icon name="x" size={13} stroke={2} />
          </button>
        </div>
      ))}
    </div>
  );
}
