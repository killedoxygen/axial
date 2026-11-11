import type { JSX } from 'preact';
import { useEffect, useRef } from 'preact/hooks';
import { api } from '../api';
import type { BlockTexturesResponse } from '../generated/BlockTexturesResponse';
import { themeSignal } from '../theme';
import type { TexturePack } from './textures';
import type { World } from './world';

/** The engine loads on first use, off the app's start-up path. */
const engine = (): Promise<[typeof import('./world'), typeof import('./render')]> =>
  Promise.all([import('./world'), import('./render')]);

/** How many pixels a picture may have, by how much machine there is. Larger boxes are drawn smaller and scaled up. */
function pixelBudget(): number {
  const cores = navigator.hardwareConcurrency || 4;
  return cores <= 4 ? 420_000 : cores <= 8 ? 900_000 : 1_400_000;
}

const SLICE_MS = 7;
const KEEP = 4;
const pictures = new Map<string, ImageData>();
let lastWorld: { key: string; world: World | null } | null = null;
/** Pictures are drawn one after another, so two requests never share the thread's slices. */
let queue: Promise<unknown> = Promise.resolve();
/** Each version's own block textures, read once. A version whose files are not there yet is asked again later. */
const packs = new Map<string, Promise<TexturePack | null>>();

interface Request {
  cancelled: boolean;
}

const pause = (): Promise<void> => new Promise((resolve) => setTimeout(resolve));

function textures(version: string): Promise<TexturePack | null> {
  let pack = packs.get(version);
  if (!pack) {
    pack = api('GET', `/versions/${encodeURIComponent(version)}/block-textures`).then(
      (response) => {
        const pack = new Map<string, Uint8Array>();
        for (const { name, rgba } of (response as BlockTexturesResponse).textures) {
          const texels = Uint8Array.from(atob(rgba), (char) => char.charCodeAt(0));
          if (texels.length === 1024) pack.set(name, texels);
        }
        return pack;
      },
      () => {
        packs.delete(version);
        return null;
      },
    );
    packs.set(version, pack);
  }
  return pack;
}

/** Draws in slices, giving the thread back between them. Resolves null once `request` is cancelled or there is nothing to draw with. */
async function draw(
  request: Request,
  seed: number,
  version: string,
  dark: boolean,
  width: number,
  height: number,
): Promise<ImageData | null> {
  const worldKey = `${seed}:${version}:${dark}`;
  const key = `${worldKey}:${width}x${height}`;
  const kept = pictures.get(key);
  if (kept) {
    pictures.delete(key);
    pictures.set(key, kept);
    return kept;
  }
  const [pack, [{ buildWorld }, { createRenderer }]] = await Promise.all([textures(version), engine()]);
  await pause();
  if (request.cancelled || !pack) return null;
  if (lastWorld?.key !== worldKey) lastWorld = { key: worldKey, world: buildWorld(seed, dark, pack) };
  const world = lastWorld.world;
  await pause();
  if (request.cancelled || !world) return null;
  const renderer = createRenderer(world, width, height);
  for (let x = 0; x < width;) {
    await pause();
    if (request.cancelled) return null;
    const started = performance.now();
    do {
      const to = Math.min(width, x + 8);
      renderer.columns(x, to);
      x = to;
    } while (x < width && performance.now() - started < SLICE_MS);
  }
  const finishing = renderer.finishing();
  for (;;) {
    await pause();
    if (request.cancelled) return null;
    const step = finishing.next();
    if (!step.done) continue;
    const picture = new ImageData(step.value, width, height);
    pictures.set(key, picture);
    if (pictures.size > KEEP) pictures.delete(pictures.keys().next().value!);
    return picture;
  }
}

/**
 * An instance's world, drawn from its art seed with its own version's block textures: sunset to night in dark mode,
 * daylight in light. Until the version's files are there, nothing is shown; `installing` turning false asks again.
 * The picture has no frame; its stylesheet anchors it to the panel's corner and thins it out into the page.
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

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas || installing) return undefined;
    // The newest request owns the canvas; an older one is cancelled and may not paint.
    let current: Request | null = null;
    let settle = 0;
    const paint = (): void => {
      const box = canvas.getBoundingClientRect();
      if (box.width < 16 || box.height < 16) return;
      const scale = Math.min(window.devicePixelRatio || 1, Math.sqrt(pixelBudget() / (box.width * box.height)));
      // Whole steps of 16 so a nudge of the window edge reuses the picture it already has.
      const width = Math.max(160, Math.round((box.width * scale) / 16) * 16);
      const height = Math.max(64, Math.round((width * box.height) / box.width));
      if (current) current.cancelled = true;
      const request: Request = { cancelled: false };
      current = request;
      const drawn = queue.then(() => (request.cancelled ? null : draw(request, seed, version, dark, width, height)));
      queue = drawn.catch(() => null);
      void drawn.then((picture) => {
        if (request.cancelled || !picture) return;
        canvas.width = picture.width;
        canvas.height = picture.height;
        canvas.getContext('2d')?.putImageData(picture, 0, 0);
        canvas.dataset.ready = 'true';
      });
    };
    paint();
    const observer = new ResizeObserver(() => {
      window.clearTimeout(settle);
      settle = window.setTimeout(paint, 180);
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
