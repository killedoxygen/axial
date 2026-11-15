import type { JSX } from 'preact';
import { useRef } from 'preact/hooks';
import { Button, IconButton } from './Atoms';
import { Face, type Eyes, type FaceTone } from './Face';
import { toasts } from '../store';
import { showOnboardingOverlay } from '../ui-state';
import { dismissToast, holdToast } from '../toast';
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
      data-modal-reachable
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
  // Focus reached a notice from somewhere, often an open dialog; closing the notice hands it back there.
  const cameFrom = useRef<HTMLElement | null>(null);
  const close = (control: EventTarget | null): void => {
    if (control instanceof Element && control.contains(document.activeElement)) cameFrom.current?.focus();
    dismissToast(toast.id);
  };
  const dismiss = <IconButton icon="x" size={28} tooltip="Dismiss" onClick={(e) => close(e.currentTarget)} />;
  if (!toast.detail && !toast.actions) {
    return (
      <div ref={leaving} class="cp-toast cp-pop" role="status">
        <Face {...FACE_FOR[toast.type]} />
        <span class="cp-toast-msg" title={toast.message}>
          {toast.message}
        </span>
        {dismiss}
      </div>
    );
  }
  // One that says more or offers something: a headline, a quieter line, then what can be done, in the copy's column.
  return (
    <div
      ref={leaving}
      class="cp-toast cp-toast--full cp-pop"
      role="status"
      onMouseEnter={() => holdToast(toast.id, 'pointer', true)}
      onMouseLeave={() => holdToast(toast.id, 'pointer', false)}
      onFocusIn={(e) => {
        const from = e.relatedTarget;
        if (from instanceof HTMLElement && !e.currentTarget.contains(from)) cameFrom.current = from;
        holdToast(toast.id, 'focus', true);
      }}
      onFocusOut={(e) => {
        if (!e.currentTarget.contains(e.relatedTarget as Node | null)) holdToast(toast.id, 'focus', false);
      }}
    >
      <Face {...FACE_FOR[toast.type]} />
      <div class="cp-toast-copy">
        <strong>{toast.message}</strong>
        {toast.detail && <p>{toast.detail}</p>}
        {toast.actions && (
          <div class="cp-toast-actions">
            {toast.actions.map((action) => (
              <Button
                key={action.label}
                variant={action.primary ? 'primary' : 'ghost'}
                size="sm"
                title={action.title}
                onClick={(e) => {
                  close(e.currentTarget);
                  action.run();
                }}
              >
                {action.label}
              </Button>
            ))}
          </div>
        )}
      </div>
      {dismiss}
    </div>
  );
}
