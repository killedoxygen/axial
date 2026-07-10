import type { ComponentChildren, JSX } from 'preact';
import { createPortal } from 'preact/compat';

export function FloatingPill({
  children,
  role = 'toolbar',
  ariaLabel,
}: {
  children: ComponentChildren;
  role?: JSX.AriaRole;
  ariaLabel?: string;
}): JSX.Element {
  return createPortal(
    <div class="cp-fpill-float" aria-live="polite">
      <div class="cp-fpill" role={role} aria-label={ariaLabel}>
        {children}
      </div>
    </div>,
    document.body,
  );
}

export function FloatingPillDivider(): JSX.Element {
  return <span class="cp-fpill-divider" aria-hidden="true" />;
}
