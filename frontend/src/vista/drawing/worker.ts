import { pack, unpack } from './packing';
import { createRenderer, type Renderer } from './render';
import type { Quarter, Strip } from './strips';
import type { TexturePack } from './textures';
import { buildWorld, type World } from './world';

/** This module is the worker's own script: a page reads where it lives, a worker runs it. */
export const url = import.meta.url;

/** A picture's pixels and size. */
export interface Picture {
  pixels: ImageData['data'];
  width: number;
  height: number;
}

/**
 * A world to build, a picture of a world to turn to, a strip of it to draw, what to finish its strips with, a
 * finished picture to pack for the store, or a packed one to unpack.
 */
export type Order =
  | { seed: number; dark: boolean; pack: TexturePack }
  /** `horizon` is the row the horizon lies on, as a share of the height. */
  | { world: World; width: number; height: number; horizon: number | undefined }
  | { from: number; to: number }
  /** The whole picture's bloom and light-shaft sources. */
  | { soft: Float32Array; open: Float32Array }
  | Picture
  | { file: ArrayBuffer };
/**
 * The world built, or null with nothing to build it from; true once turned to a picture; a strip's quarter; the
 * finished strips; the picture packed; the picture unpacked.
 */
export type Reply = World | null | true | Quarter | Strip[] | ArrayBuffer | Picture;

if (typeof document === 'undefined') {
  const port = self as unknown as {
    onmessage: (event: MessageEvent<Order>) => void;
    postMessage(reply: Reply, transfer?: Transferable[]): void;
  };
  let renderer: Renderer;
  port.onmessage = ({ data }) => {
    if ('pixels' in data) {
      const { buffer } = pack(data.pixels, data.width, data.height);
      return port.postMessage(buffer, [buffer]);
    }
    if ('file' in data) {
      const picture = unpack(new Uint8Array(data.file));
      return port.postMessage(picture, [picture.pixels.buffer]);
    }
    if ('pack' in data) return port.postMessage(buildWorld(data.seed, data.dark, data.pack));
    if ('world' in data) {
      renderer = createRenderer(data.world, data.width, data.height, data.horizon);
      return port.postMessage(true);
    }
    if ('from' in data) {
      const part = renderer.draw(data.from, data.to);
      return port.postMessage(part, [part.soft.buffer, part.open.buffer]);
    }
    const strips = renderer.finish(data.soft, data.open);
    port.postMessage(
      strips,
      strips.map((strip) => strip.pixels.buffer),
    );
  };
}
