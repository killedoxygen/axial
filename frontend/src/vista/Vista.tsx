import type { JSX } from 'preact';
import { useLayoutEffect, useRef } from 'preact/hooks';

/** The pictures are their own chunk, off the app's first script. Once loaded they show a kept picture at once. */
let pictures: typeof import('./pictures') | undefined;
const noop = (): void => undefined;
const loading = (): Promise<typeof import('./pictures')> => import('./pictures').then((loaded) => (pictures = loaded));

/**
 * Has the first seed's picture ready before the place that shows it is on screen, and the rest drawn in the
 * background unless the machine is `busy`. Never rejects.
 */
export function warmVistas(seeds: number[], busy: boolean): Promise<void> {
  return loading().then((loaded) => loaded.warm(seeds, busy), noop);
}

/**
 * An instance's world, from its art seed: water at dusk, a lit building on its far shore, in one of the game's
 * biomes. A picture already drawn is on screen before the first paint; a new one fades in when it is done.
 */
export function Vista({ seed }: { seed: number }): JSX.Element {
  const canvasRef = useRef<HTMLCanvasElement>(null);

  useLayoutEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return undefined;
    let stop: (() => void) | undefined;
    let over = false;
    const start = (): void => {
      if (!over) stop = pictures!.show(canvas, seed);
    };
    if (pictures) start();
    else loading().then(start, noop);
    return () => {
      over = true;
      stop?.();
    };
  }, [seed]);

  return <canvas ref={canvasRef} class="cp-vista" aria-hidden="true" />;
}
