import type { JSX } from 'preact';
import { IconButton } from './Atoms';
import { Face, type Eyes, type FaceTone } from './Face';
import { toasts } from '../store';
import { showOnboardingOverlay } from '../ui-state';
import { dismissToast } from '../toast';
import type { ToastKind } from '../types-ui';

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
    <div class="cp-toasts" role="region" aria-live="polite" aria-label="Notifications">
      {list.map((t) => (
        <div key={t.id} class="cp-toast cp-pop" role="status">
          <Face {...FACE_FOR[t.type]} />
          <span class="cp-toast-msg" title={t.message}>
            {t.message}
          </span>
          <IconButton icon="x" size={28} tooltip="Dismiss" onClick={() => dismissToast(t.id)} />
        </div>
      ))}
    </div>
  );
}
