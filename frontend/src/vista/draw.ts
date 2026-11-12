import { api } from '../api';
import type { BlockTexturesResponse } from '../generated/BlockTexturesResponse';
import { createRenderer } from './render';
import type { TexturePack } from './textures';
import { buildWorld } from './world';

const SLICE_MS = 10;
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
 * there is nothing to draw with.
 */
export async function draw(
  request: { cancelled: boolean },
  seed: number,
  version: string,
  dark: boolean,
  width: number,
  height: number,
): Promise<ImageData | null> {
  const pack = await textures(version);
  await pause();
  if (request.cancelled || !pack) return null;
  const world = buildWorld(seed, dark, pack);
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
    return new ImageData(step.value, width, height);
  }
}
