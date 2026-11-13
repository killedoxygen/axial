import type { JSX } from 'preact';
import { useLayoutEffect, useRef } from 'preact/hooks';
import { themeSignal } from '../theme';

/** The drawing loads on first use, off the app's start-up path. Once loaded it can show a kept picture at once. */
let studio: typeof import('./draw') | undefined;

/**
 * An instance's world, drawn from its art seed with its own version's block textures: sunset to night in dark mode,
 * daylight in light. Until the version's files are there, nothing is shown; `installing` turning false asks again.
 * A picture already drawn is on screen before the first paint; a new one fades in when it is done.
 */
export function Vista({
  seed,
  version,
  installing,
}: {
  seed: number;
  version: string;
  installing: boolean;
}): JSX.Element {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const { dark } = themeSignal.value;

  useLayoutEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas || installing) return undefined;
    let stop: (() => void) | null | undefined;
    const start = (): void => {
      if (stop !== null) stop = studio!.show(canvas, seed, version, dark);
    };
    if (studio) start();
    else {
      void import('./draw').then(
        (loaded) => {
          studio = loaded;
          start();
        },
        () => undefined,
      );
    }
    return () => {
      stop?.();
      stop = null;
    };
  }, [seed, version, installing, dark]);

  return <canvas ref={canvasRef} class="cp-vista" aria-hidden="true" />;
}
