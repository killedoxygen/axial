import type { JSX } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { startApplicationBootstrap } from '../bootstrap';
import { bootstrapError, bootstrapState } from '../store';
import { Logo } from '../ui/Logo';

const MIN_DISPLAY_MS = 500;
const FILL_SETTLE_MS = 240;
const LEAVE_MS = 360;

type BootLogoStyle = JSX.CSSProperties & Record<string, string>;

function clamp01(value: number): number {
  return Math.min(1, Math.max(0, value));
}

function progressSegment(progress: number, start: number, end: number): number {
  return clamp01((progress - start) / (end - start));
}

// The face stays opaque so the gill roots beneath it never show through while they fade in.
function bootLogoStyle(progress: number): BootLogoStyle {
  const face = progressSegment(progress, 0, 35);
  const left = progressSegment(progress, 14, 58);
  const right = progressSegment(progress, 24, 72);

  return {
    '--cp-mark-assembly-face-scale': String(0.9 + face * 0.1),
    '--cp-mark-assembly-left-opacity': String(left),
    '--cp-mark-assembly-left-scale': String(0.8 + left * 0.2),
    '--cp-mark-assembly-right-opacity': String(right),
    '--cp-mark-assembly-right-scale': String(0.8 + right * 0.2),
  };
}

export function BootSplash(): JSX.Element | null {
  const state = bootstrapState.value;
  const [progress, setProgress] = useState(4);
  const [leaving, setLeaving] = useState(false);
  const [gone, setGone] = useState(false);
  const mountedAt = useRef(Date.now());

  useEffect(() => {
    if (state !== 'loading') return;
    mountedAt.current = Date.now();
    setProgress(4);
    setLeaving(false);
    const tick = setInterval(() => {
      setProgress((p) => Math.min(90, p + Math.max(0.4, (90 - p) * 0.045)));
    }, 90);
    return () => clearInterval(tick);
  }, [state]);

  useEffect(() => {
    if (state !== 'ready') return;
    setProgress(100);
    const elapsed = Date.now() - mountedAt.current;
    const leaveTimer = setTimeout(() => setLeaving(true), Math.max(FILL_SETTLE_MS, MIN_DISPLAY_MS - elapsed));
    return () => clearTimeout(leaveTimer);
  }, [state]);

  useEffect(() => {
    if (!leaving) return;
    const goneTimer = setTimeout(() => setGone(true), LEAVE_MS);
    return () => clearTimeout(goneTimer);
  }, [leaving]);

  if (gone) return null;

  return (
    <div
      class="cp-boot cp-drag"
      data-leaving={leaving || undefined}
      role={state === 'error' ? 'alert' : 'status'}
      aria-live={state === 'error' ? 'assertive' : 'polite'}
    >
      <div class="cp-boot-stack">
        <Logo className="cp-boot-logo" motion="assembly" size={64} style={bootLogoStyle(progress)} />
        {state === 'error' ? (
          <>
            <div class="cp-boot-error-title">Failed to connect</div>
            <div class="cp-boot-error-msg">
              {bootstrapError.value || 'The launcher could not load its initial state.'}
            </div>
            <button class="cp-btn cp-btn--primary cp-btn--sm" type="button" onClick={startApplicationBootstrap}>
              Retry
            </button>
          </>
        ) : (
          <>
            <div class="cp-boot-bar">
              <div class="cp-boot-bar-fill" style={{ width: `${progress}%` }} />
            </div>
            <div class="cp-boot-label">Starting Axial…</div>
          </>
        )}
      </div>
    </div>
  );
}
