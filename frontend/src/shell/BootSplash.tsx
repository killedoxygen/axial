import type { JSX } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { bootstrapError, bootstrapState } from '../store';
import { hasNativeDesktopRuntime, windowStartDragging } from '../native';
import { Logo } from '../ui/Logo';

const MIN_DISPLAY_MS = 500;
const FILL_SETTLE_MS = 240;
const LEAVE_MS = 360;

export function BootSplash(): JSX.Element | null {
  const state = bootstrapState.value;
  const [progress, setProgress] = useState(4);
  const [leaving, setLeaving] = useState(false);
  const [gone, setGone] = useState(false);
  const mountedAt = useRef(Date.now());
  const isNative = useRef(hasNativeDesktopRuntime());

  useEffect(() => {
    if (state !== 'loading') return;
    const tick = setInterval(() => {
      setProgress((p) => Math.min(90, p + Math.max(0.4, (90 - p) * 0.045)));
    }, 90);
    return () => clearInterval(tick);
  }, [state]);

  useEffect(() => {
    if (state !== 'ready') return;
    setProgress(100);
    const elapsed = Date.now() - mountedAt.current;
    const leaveTimer = setTimeout(
      () => setLeaving(true),
      Math.max(FILL_SETTLE_MS, MIN_DISPLAY_MS - elapsed),
    );
    return () => clearTimeout(leaveTimer);
  }, [state]);

  useEffect(() => {
    if (!leaving) return;
    const goneTimer = setTimeout(() => setGone(true), LEAVE_MS);
    return () => clearTimeout(goneTimer);
  }, [leaving]);

  if (gone) return null;

  const onMouseDown = (e: MouseEvent): void => {
    if (!isNative.current || e.button !== 0) return;
    void windowStartDragging();
  };

  return (
    <div
      class="cp-boot"
      data-leaving={leaving || undefined}
      role="status"
      aria-live="polite"
      onMouseDown={onMouseDown}
    >
      <div class="cp-boot-stack">
        <Logo className="cp-boot-logo" size={56} />
        {state === 'error' ? (
          <>
            <div class="cp-boot-error-title">Failed to connect</div>
            <div class="cp-boot-error-msg">
              {bootstrapError.value || 'The launcher could not load its initial state.'}
            </div>
          </>
        ) : (
          <>
            <div class="cp-boot-bar">
              <div class="cp-boot-bar-fill" style={{ width: `${progress}%` }} />
            </div>
            <div class="cp-boot-label">Starting Croopor…</div>
          </>
        )}
      </div>
    </div>
  );
}
