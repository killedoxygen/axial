import { api } from '../api';
import type { BlockTexturesResponse } from '../generated/BlockTexturesResponse';
import { Sheet, type Quarter, type Strip } from './drawing/strips';
import type { TexturePack } from './drawing/textures';
import type { Order, Picture, Reply } from './drawing/worker';
import type { World } from './drawing/world';

/** An instance's world in a colour mode. */
export interface Scene {
  seed: number;
  version: string;
  dark: boolean;
}

/**
 * Pictures are drawn off the page's thread by a crew of workers: half the machine's threads, which is about its
 * fast cores. More only get in each other's way.
 */
const CREW = Math.max(1, Math.min(6, Math.ceil((navigator.hardwareConcurrency || 4) / 2)));
/** A worker is worth setting to a picture for every this many pixels of it. */
const PIXELS_EACH = 150_000;
/** The fewest columns worth a message of their own. */
const NARROWEST = 32;
/** A crew with nothing to draw for this long is let go, and with it the last world built. */
const IDLE_MS = 20_000;

const hired = (count: number): Promise<Worker[]> =>
  import('./drawing/worker').then(({ url }) =>
    Array.from({ length: count }, () => new Worker(url, { type: 'module' })),
  );

let crew: Promise<Worker[]> | null = null;
/** The last world built: a scene is often drawn again at once, as a band or for a larger place. */
let built: { scene: string; world: World } | null = null;
let leaving = 0;

function dismiss(): void {
  crew?.then(
    (workers) => workers.forEach((worker) => worker.terminate()),
    () => undefined,
  );
  crew = null;
  built = null;
}

/** One order at a time for each worker, so a reply needs no name. */
function ask<T extends Reply>(worker: Worker, order: Order, transfer: Transferable[] = []): Promise<T> {
  return new Promise((resolve, reject) => {
    worker.onmessage = (event: MessageEvent<T>) => resolve(event.data);
    worker.onerror = reject;
    worker.postMessage(order, transfer);
  });
}

/** Each version's own block textures, read once. A version whose files are not there yet is asked again later. */
const packs = new Map<string, Promise<TexturePack | null>>();

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
 * Draws a scene `width` by `height`. One worker builds the world; then every worker set to the picture takes
 * strips of columns until none are left, and finishes its own with the whole picture's bloom and light shafts.
 * Resolves null once `stopped` says so or there is nothing to draw with. `horizon` is the row the horizon
 * lies on, as a share of the height.
 */
export async function draw(
  stopped: () => boolean,
  { seed, version, dark }: Scene,
  width: number,
  height: number,
  horizon?: number,
): Promise<ImageData | null> {
  window.clearTimeout(leaving);
  try {
    const [pack, all] = await Promise.all([textures(version), (crew ??= hired(CREW))]);
    if (!pack || stopped()) return null;
    const scene = `${seed}:${version}:${dark}`;
    if (built?.scene !== scene) {
      const world = await ask<World | null>(all[0]!, { seed, dark, pack });
      if (!world) return null;
      built = { scene, world };
    }
    const { world } = built;
    const workers = all.slice(0, Math.ceil((width * height) / PIXELS_EACH));
    const sheet = new Sheet(width, height);
    const step = Math.max(NARROWEST, Math.ceil(width / (workers.length * 16)) * 4);
    let next = 0;
    await Promise.all(
      workers.map(async (worker) => {
        await ask(worker, { world, width, height, horizon });
        while (next < width && !stopped()) {
          const from = next;
          next = Math.min(width, from + step);
          sheet.lay(await ask<Quarter>(worker, { from, to: next }));
        }
      }),
    );
    if (stopped()) return null;
    await Promise.all(
      workers.map(async (worker) => {
        for (const strip of await ask<Strip[]>(worker, { soft: sheet.soft, open: sheet.open })) sheet.set(strip);
      }),
    );
    return new ImageData(sheet.pixels, width, height);
  } catch {
    dismiss();
    return null;
  } finally {
    leaving = window.setTimeout(dismiss, IDLE_MS);
  }
}

/**
 * One more worker, there from the start and kept, packs pictures for the store and unpacks them, one at a time:
 * neither a drawing nor the page waits on it, and a launch that only reads pictures needs no crew.
 */
const clerk = hired(1);
let desk: Promise<unknown> = Promise.resolve();

function filed<T extends Reply>(order: Order, transfer: Transferable[] = []): Promise<T | null> {
  const reply = desk
    .then(() => clerk)
    .then(([worker]) => ask<T>(worker!, order, transfer))
    .catch(() => null);
  desk = reply;
  return reply;
}

/** A picture as the store holds it: a fraction of the size and nothing lost. Null where it could not be packed. */
export const packed = ({ data, width, height }: ImageData): Promise<ArrayBuffer | null> =>
  filed({ pixels: data, width, height });

/** The picture a stored file holds, or undefined where it cannot be read. Takes the file with it. */
export const unpacked = (file: ArrayBuffer): Promise<ImageData | undefined> =>
  filed<Picture>({ file }, [file]).then((picture) =>
    picture ? new ImageData(picture.pixels, picture.width, picture.height) : undefined,
  );
