import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { toast } from '../toast';
import { errMessage } from '../utils';
import {
  closeWindow,
  drawsWindowControls,
  isWindowMaximized,
  minimizeWindow,
  ownsWindowFrame,
  toggleWindowMaximized,
} from './window';

const GLYPHS = {
  minimize: 'M0.5 5.5h9',
  maximize: 'M1 1h8v8H1z',
  restore: 'M3 3V1h6v6H7M1 3h6v6H1z',
  close: 'M1 1l8 8M9 1L1 9',
};

function Glyph({ name }: { name: keyof typeof GLYPHS }): JSX.Element {
  return (
    <svg width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor" aria-hidden="true">
      <path d={GLYPHS[name]} />
    </svg>
  );
}

/** Minimise, maximise and close, where the launcher draws them: Windows and Linux, each in its own manner. */
export function WindowControls(): JSX.Element | null {
  const drawn = drawsWindowControls();
  const [maximized, setMaximized] = useState(false);

  useEffect(() => {
    if (!drawn) return undefined;
    const sync = (): void => {
      void isWindowMaximized()
        .then(setMaximized)
        .catch(() => undefined);
    };
    sync();
    window.addEventListener('resize', sync);
    return () => window.removeEventListener('resize', sync);
  }, [drawn]);

  if (!drawn) return null;
  const report = (error: unknown): void => toast(errMessage(error), 'error');

  return (
    <div class="cp-winctrls cp-nodrag">
      <button
        type="button"
        class="cp-winctrl"
        aria-label="Minimize"
        onClick={() => void minimizeWindow().catch(report)}
      >
        <Glyph name="minimize" />
      </button>
      <button
        type="button"
        class="cp-winctrl"
        aria-label={maximized ? 'Restore' : 'Maximize'}
        onClick={() => void toggleWindowMaximized().then(setMaximized).catch(report)}
      >
        <Glyph name={maximized ? 'restore' : 'maximize'} />
      </button>
      <button
        type="button"
        class="cp-winctrl"
        data-close="true"
        aria-label="Close"
        onClick={() => void closeWindow().catch(report)}
      >
        <Glyph name="close" />
      </button>
    </div>
  );
}

/**
 * For a surface that covers the whole frame (the boot splash, onboarding): the strip along its top edge that the
 * window is dragged by, carrying the window's buttons where the launcher draws them.
 */
export function WindowStrip(): JSX.Element | null {
  if (!ownsWindowFrame()) return null;
  return (
    <div class="cp-window-strip cp-drag">
      <WindowControls />
    </div>
  );
}
