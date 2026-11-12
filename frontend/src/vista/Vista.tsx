import type { JSX } from 'preact';
import { useLayoutEffect, useRef } from 'preact/hooks';
import { themeSignal } from '../theme';

/**
 * One picture is drawn for an instance in a colour mode and every place that shows it shares it: Home, the instance
 * page, and the same place at another window size. Each place scales and crops it in its stylesheet, so nothing
 * is drawn again for a change of page or of size. It is drawn as wide as the place that first asks for it, and
 * drawn again only when a place is so much wider that scaling would soften it. The drawing itself loads on first
 * use, off the app's start-up path.
 */
const ASPECT = 2.3;
const PIXELS = 2_400_000;
const WIDEST = Math.floor(Math.sqrt(PIXELS * ASPECT));
const KEEP = 6;
const pictures = new Map<string, ImageData>();
/** Pictures are drawn one after another, so two requests never share the thread's slices. */
let queue: Promise<unknown> = Promise.resolve();

/** The picture kept for a key, if it is sharp enough at `width`. */
function kept(key: string, width: number): ImageData | undefined {
  const picture = pictures.get(key);
  return picture && picture.width >= Math.min(WIDEST, width) * 0.8 ? picture : undefined;
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
    const key = `${seed}:${version}:${dark}`;
    let current: { cancelled: boolean } | null = null;
    let settle = 0;
    const show = (picture: ImageData, ready: string): void => {
      canvas.width = picture.width;
      canvas.height = picture.height;
      canvas.getContext('2d')?.putImageData(picture, 0, 0);
      canvas.dataset.ready = ready;
    };
    const paint = (): void => {
      const wanted = canvas.getBoundingClientRect().width * (window.devicePixelRatio || 1);
      if (wanted < 16) return;
      const ready = kept(key, wanted);
      if (ready) {
        if (canvas.width !== ready.width || !canvas.dataset.ready) show(ready, canvas.dataset.ready ?? 'kept');
        return;
      }
      // Meanwhile a softer picture of the same scene stands in, if there is one.
      const softer = pictures.get(key);
      if (softer && !canvas.dataset.ready) show(softer, 'kept');
      if (current) current.cancelled = true;
      const request = { cancelled: false };
      current = request;
      const width = Math.min(WIDEST, Math.max(320, Math.ceil(wanted / 64) * 64));
      const drawn = queue.then(async () => {
        if (request.cancelled) return;
        const picture =
          kept(key, wanted) ??
          (await (await import('./draw')).draw(request, seed, version, dark, width, Math.round(width / ASPECT)));
        if (!picture) return;
        pictures.delete(key);
        pictures.set(key, picture);
        if (pictures.size > KEEP) pictures.delete(pictures.keys().next().value!);
        if (!request.cancelled) show(picture, canvas.dataset.ready ?? 'drawn');
      });
      queue = drawn.catch(() => null);
    };
    delete canvas.dataset.ready;
    paint();
    const observer = new ResizeObserver(() => {
      window.clearTimeout(settle);
      settle = window.setTimeout(paint, 400);
    });
    observer.observe(canvas);
    return () => {
      if (current) current.cancelled = true;
      window.clearTimeout(settle);
      observer.disconnect();
    };
  }, [seed, version, installing, dark]);

  return <canvas ref={canvasRef} class="cp-vista" aria-hidden="true" />;
}
