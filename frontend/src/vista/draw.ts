import { api } from '../api';
import type { BlockTexturesResponse } from '../generated/BlockTexturesResponse';
import { createRenderer } from './render';
import type { TexturePack } from './textures';
import { buildWorld } from './world';

const SLICE_MS = 10;
const ASPECT = 2.3;
const PIXELS = 2_400_000;
const WIDEST = Math.floor(Math.sqrt(PIXELS * ASPECT));
/** A place this wide or narrower, in device pixels, gets a picture of its own size: a large one shrinks badly. */
const SMALL = 960;
/** What the kept pictures may hold between them: six of the largest. */
const KEPT_BYTES = PIXELS * 4 * 6;
/**
 * One whole picture is kept for a scene (an instance's world in a colour mode) and every large place that shows it
 * shares it: Home, the instance page, and the same place at another window size. Each scales and crops it in its
 * stylesheet, so a change of page or of size shows a picture at once. It is drawn as wide as the place that first
 * asks, and again only for a place so much wider that scaling would soften it.
 *
 * Where the widest whole picture is still soft, a shallow place then gets a band: just the rows it shows, at its
 * own sharpness. The band's key carries where the place sets the picture, as its stylesheet's `object-position`.
 */
const kept = new Map<string, ImageData>();
/** Pictures are drawn one after another, so two requests never share the thread's slices. */
let queue: Promise<unknown> = Promise.resolve();
/** Each version's own block textures, read once. A version whose files are not there yet is asked again later. */
const packs = new Map<string, Promise<TexturePack | null>>();

/** Gives the thread back for whatever is waiting on it, and takes it again at once; a timer would idle for milliseconds. */
let channel: MessageChannel | null = null;
let resume: (() => void) | null = null;
const pause = (): Promise<void> =>
  new Promise((resolve) => {
    if (!channel) {
      channel = new MessageChannel();
      channel.port1.onmessage = () => resume?.();
    }
    resume = resolve;
    channel.port2.postMessage(0);
  });

function textures(version: string): Promise<TexturePack | null> {
  let pack = packs.get(version);
  if (!pack) {
    pack = api('GET', `/versions/${encodeURIComponent(version)}/block-textures`).then(
      (response) => {
        const pack = new Map<string, Uint8Array>();
        for (const { name, rgba } of (response as BlockTexturesResponse).textures) {
          const texels = Uint8Array.from(atob(rgba), (char) => char.charCodeAt(0));
          if (texels.length === (name === 'sun' || name === 'moon' ? 4096 : 1024)) pack.set(name, texels);
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

/**
 * Draws a picture in slices, giving the thread back between them. Resolves null once `request` is cancelled or
 * there is nothing to draw with. `horizon` is the row the horizon lies on, as a share of the height.
 */
async function draw(
  request: { cancelled: boolean },
  seed: number,
  version: string,
  dark: boolean,
  width: number,
  height: number,
  horizon?: number,
): Promise<ImageData | null> {
  const pack = await textures(version);
  await pause();
  if (request.cancelled || !pack) return null;
  const world = buildWorld(seed, dark, pack);
  await pause();
  if (request.cancelled || !world) return null;
  const renderer = createRenderer(world, width, height, horizon);
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
    return new ImageData(step.value, width, height);
  }
}

function keep(key: string, picture: ImageData): void {
  kept.delete(key);
  kept.set(key, picture);
  let bytes = 0;
  for (const each of kept.values()) bytes += each.data.length;
  for (const [oldest, each] of kept) {
    if (bytes <= KEPT_BYTES || oldest === key) break;
    bytes -= each.data.length;
    kept.delete(oldest);
  }
}

/** Whether `picture` is too shallow to hold the rows a place of `shape` shows. */
const lacks = (picture: ImageData, shape: number): boolean => picture.width > picture.height * shape * 1.002;

/** Whether `picture` is sharp at `width` and, for a band, holds the rows a place of `shape` shows. */
const serves = (picture: ImageData | undefined, width: number, shape = ASPECT): picture is ImageData =>
  !!picture && picture.width >= width * 0.8 && !lacks(picture, shape);

/**
 * Shows a scene on `canvas` and keeps it right for the canvas's size until the returned function is called. A kept
 * picture is on the canvas before this returns; a softer one stands in while a sharper one is drawn.
 */
export function show(canvas: HTMLCanvasElement, seed: number, version: string, dark: boolean): () => void {
  const scene = `${seed}:${version}:${dark}`;
  let request: { cancelled: boolean } | null = null;
  let shown: ImageData | undefined;
  let settle = 0;
  const put = (picture: ImageData, ready: string): void => {
    if (shown !== picture) {
      shown = picture;
      canvas.width = picture.width;
      canvas.height = picture.height;
      canvas.getContext('2d')?.putImageData(picture, 0, 0);
    }
    canvas.dataset.ready ??= ready;
  };
  const paint = (): void => {
    const box = canvas.getBoundingClientRect();
    const shape = box.width / box.height;
    // How wide the whole picture stands here: a place narrower than the picture crops its sides.
    const wanted = Math.max(box.width, box.height * ASPECT) * (window.devicePixelRatio || 1);
    if (!(wanted >= 16)) return;
    const wholeKey = wanted > SMALL ? scene : `${scene}:small`;
    const whole = kept.get(wholeKey);
    let key = wholeKey;
    let width = Math.min(WIDEST, Math.max(320, Math.ceil(wanted / 64) * 64));
    let height = Math.round(width / ASPECT);
    let horizon: number | undefined;
    if (serves(whole, Math.min(WIDEST, wanted))) {
      put(whole, 'kept');
      // The band: worth drawing where the whole picture is stretched and the place shows well under all of it.
      if (whole.width >= wanted * 0.8 || shape < ASPECT * 1.3) return;
      const anchor = parseFloat(getComputedStyle(canvas).objectPosition.split(' ')[1]!) / 100;
      const wide = Math.min(Math.ceil(wanted / 64), Math.floor(Math.sqrt(PIXELS * shape) / 64)) * 64;
      const rows = Math.round(wide / shape);
      const lift = (wide / ASPECT / 2 - anchor * (wide / ASPECT - rows)) / rows;
      if (!(lift > 0 && lift < 1)) return;
      key = `${scene}:${anchor}`;
      const band = kept.get(key);
      if (serves(band, wide, shape)) return put(band, 'kept');
      [width, height, horizon] = [wide, rows, lift];
    } else if (whole) {
      put(whole, 'kept');
    }
    if (request) request.cancelled = true;
    const mine = (request = { cancelled: false });
    const [toKey, toWidth, toHeight, toHorizon] = [key, width, height, horizon];
    const drawn = queue.then(async () => {
      if (mine.cancelled) return;
      const ready = kept.get(toKey);
      const picture = serves(ready, toWidth, toHorizon === undefined ? ASPECT : shape)
        ? ready
        : await draw(mine, seed, version, dark, toWidth, toHeight, toHorizon);
      if (!picture) return;
      keep(toKey, picture);
      if (mine.cancelled) return;
      put(picture, 'drawn');
      if (toHorizon === undefined) paint();
    });
    queue = drawn.catch(() => null);
  };
  delete canvas.dataset.ready;
  paint();
  const observer = new ResizeObserver(() => {
    // A band holds only the rows its place showed. A place grown deeper goes back to the whole picture at once.
    const box = canvas.getBoundingClientRect();
    const whole = kept.get(scene);
    if (shown && whole && lacks(shown, ASPECT) && lacks(shown, box.width / box.height)) put(whole, 'kept');
    window.clearTimeout(settle);
    settle = window.setTimeout(paint, 400);
  });
  observer.observe(canvas);
  return () => {
    if (request) request.cancelled = true;
    window.clearTimeout(settle);
    observer.disconnect();
  };
}
