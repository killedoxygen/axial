import type { JSX } from 'preact';
import { useLayoutEffect, useRef } from 'preact/hooks';
import { themeSignal } from '../theme';

/** The pictures are their own chunk, off the app's first script. Once loaded they show a kept picture at once. */
let pictures: typeof import('./pictures') | undefined;
const noop = (): void => undefined;
const loading = (): Promise<typeof import('./pictures')> => import('./pictures').then((loaded) => (pictures = loaded));

/**
 * Has the first scene's picture, in the colour mode of the moment, ready before the place that shows it is on
 * screen, and the rest drawn in the background unless the machine is `busy`. Never rejects.
 */
export function warmVistas(scenes: { seed: number; version: string }[], busy: boolean): Promise<void> {
  const { dark } = themeSignal.value;
  return loading().then(
    (loaded) =>
      loaded.warm(
        scenes.map((scene) => ({ ...scene, dark })),
        busy,
      ),
    noop,
  );
}

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
    let stop: (() => void) | undefined;
    let over = false;
    const start = (): void => {
      if (!over) stop = pictures!.show(canvas, { seed, version, dark });
    };
    if (pictures) start();
    else loading().then(start, noop);
    return () => {
      over = true;
      stop?.();
    };
  }, [seed, version, installing, dark]);

  return <canvas ref={canvasRef} class="cp-vista" aria-hidden="true" />;
}
