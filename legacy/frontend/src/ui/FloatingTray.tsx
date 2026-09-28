import type { ComponentChildren, JSX } from 'preact';
import { createPortal } from 'preact/compat';
import { useLayoutEffect, useRef, useState } from 'preact/hooks';

const FLOATING_TRAY_BOTTOM_OFFSET = 18;

export function FloatingTray({
  children,
  role = 'toolbar',
  ariaLabel,
  reserveSpace = false,
}: {
  children: ComponentChildren;
  role?: JSX.AriaRole;
  ariaLabel?: string;
  reserveSpace?: boolean;
}): JSX.Element {
  const trayRef = useRef<HTMLDivElement>(null);
  const [reservedHeight, setReservedHeight] = useState(0);

  useLayoutEffect(() => {
    if (!reserveSpace) {
      setReservedHeight(0);
      return;
    }

    const tray = trayRef.current;
    if (!tray) return;

    const measure = (): void => {
      setReservedHeight(Math.ceil(tray.getBoundingClientRect().height) + FLOATING_TRAY_BOTTOM_OFFSET);
    };

    measure();
    const observer = typeof ResizeObserver !== 'undefined' ? new ResizeObserver(measure) : null;
    observer?.observe(tray);
    window.addEventListener('resize', measure);

    return () => {
      observer?.disconnect();
      window.removeEventListener('resize', measure);
    };
  }, [reserveSpace]);

  return (
    <>
      {reserveSpace && <div class="cp-floating-tray-reserve" style={{ height: reservedHeight }} aria-hidden="true" />}
      {createPortal(
        <div class="cp-floating-tray-position" aria-live="polite">
          <div ref={trayRef} class="cp-floating-tray" role={role} aria-label={ariaLabel}>
            {children}
          </div>
        </div>,
        document.body,
      )}
    </>
  );
}

export function FloatingTrayLabel({ children }: { children: ComponentChildren }): JSX.Element {
  return <span class="cp-floating-tray-label">{children}</span>;
}

export function FloatingTrayDivider(): JSX.Element {
  return <span class="cp-floating-tray-divider" aria-hidden="true" />;
}
