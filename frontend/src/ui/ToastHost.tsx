import type { JSX } from 'preact';
import { IconButton } from './Atoms';
import { Face, type Eyes, type FaceTone } from './Face';
import { toasts } from '../store';
import { showOnboardingOverlay } from '../ui-state';
import { dismissToast } from '../toast';
import { useLeave } from '../hooks/use-leave';
import type { ToastItem, ToastKind } from '../types-ui';
import { settleChildren } from './motion';

const FACE_FOR: Record<ToastKind, { eyes: Eyes; tone?: FaceTone }> = {
  success: { eyes: 'happy' },
  error: { eyes: 'x', tone: 'err' },
  info: { eyes: 'open' },
};

export function ToastHost(): JSX.Element | null {
  // Onboarding reports its own errors inline; background notices would only cover it.
  if (showOnboardingOverlay.value) return null;
  const list = toasts.value;
  return (
    <div
      class="cp-toasts"
      role="region"
      data-settles
      ref={(list) => settleChildren(list)}
      aria-live="polite"
      aria-label="Notifications"
    >
      {list.map((t) => (
        <Toast key={t.id} toast={t} />
      ))}
    </div>
  );
}

function Toast({ toast }: { toast: ToastItem }): JSX.Element {
  const leaving = useLeave<HTMLDivElement>();
  return (
    <div ref={leaving} class="cp-toast cp-pop" role="status">
      <Face {...FACE_FOR[toast.type]} />
      <span class="cp-toast-msg" title={toast.message}>
        {toast.message}
      </span>
      <IconButton icon="x" size={28} tooltip="Dismiss" onClick={() => dismissToast(toast.id)} />
    </div>
  );
}
