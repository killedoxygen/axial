/**
 * A picture packed for the store, nothing lost: each pixel is written as a run of the one before, one of the last
 * 64 seen, a small step from the one before, or in full (the QOI scheme). It needs nothing of the browser, so it
 * runs in any worker, and it packs and unpacks faster than a PNG at a little more than a PNG's size.
 */
const RUN = 0xc0;
const STEP = 0x40;
const STRIDE = 0x80;
const RGB = 0xfe;
const RGBA = 0xff;
const HEAD = 8;

const slot = (r: number, g: number, b: number, a: number): number => (r * 3 + g * 5 + b * 7 + a * 11) & 63;

export function pack(pixels: Uint8ClampedArray, width: number, height: number): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(HEAD + (pixels.length / 4) * 5);
  const head = new DataView(out.buffer);
  head.setUint32(0, width);
  head.setUint32(4, height);
  const seen = new Int32Array(64);
  let at = HEAD;
  let run = 0;
  let r = 0;
  let g = 0;
  let b = 0;
  let a = 255;
  for (let i = 0; i < pixels.length; i += 4) {
    const R = pixels[i]!;
    const G = pixels[i + 1]!;
    const B = pixels[i + 2]!;
    const A = pixels[i + 3]!;
    if (R === r && G === g && B === b && A === a) {
      if (++run === 62) {
        out[at++] = RUN | 61;
        run = 0;
      }
      continue;
    }
    if (run) {
      out[at++] = RUN | (run - 1);
      run = 0;
    }
    const where = slot(R, G, B, A);
    const pixel = (R << 24) | (G << 16) | (B << 8) | A;
    if (seen[where] === pixel) out[at++] = where;
    else {
      seen[where] = pixel;
      // Steps wrap, as a byte does.
      const dr = ((R - r) << 24) >> 24;
      const dg = ((G - g) << 24) >> 24;
      const db = ((B - b) << 24) >> 24;
      const drg = dr - dg;
      const dbg = db - dg;
      if (A !== a) {
        out[at++] = RGBA;
        out[at++] = R;
        out[at++] = G;
        out[at++] = B;
        out[at++] = A;
      } else if (dr >= -2 && dr <= 1 && dg >= -2 && dg <= 1 && db >= -2 && db <= 1) {
        out[at++] = STEP | ((dr + 2) << 4) | ((dg + 2) << 2) | (db + 2);
      } else if (dg >= -32 && dg <= 31 && drg >= -8 && drg <= 7 && dbg >= -8 && dbg <= 7) {
        out[at++] = STRIDE | (dg + 32);
        out[at++] = ((drg + 8) << 4) | (dbg + 8);
      } else {
        out[at++] = RGB;
        out[at++] = R;
        out[at++] = G;
        out[at++] = B;
      }
    }
    r = R;
    g = G;
    b = B;
    a = A;
  }
  if (run) out[at++] = RUN | (run - 1);
  return out.slice(0, at);
}

/** The picture `pack` was given. A file cut short unpacks to a picture of the right size, wrong past the cut. */
export function unpack(file: Uint8Array): { pixels: Uint8ClampedArray<ArrayBuffer>; width: number; height: number } {
  const head = new DataView(file.buffer, file.byteOffset);
  const width = head.getUint32(0);
  const height = head.getUint32(4);
  const pixels = new Uint8ClampedArray(width * height * 4);
  const seen = new Int32Array(64);
  let at = HEAD;
  let run = 0;
  let r = 0;
  let g = 0;
  let b = 0;
  let a = 255;
  for (let i = 0; i < pixels.length; i += 4) {
    if (run) run--;
    else {
      const op = file[at++]!;
      if (op === RGB || op === RGBA) {
        r = file[at++]!;
        g = file[at++]!;
        b = file[at++]!;
        if (op === RGBA) a = file[at++]!;
      } else if (op >= RUN) run = op & 63;
      else if (op >= STRIDE) {
        const more = file[at++]!;
        const dg = (op & 63) - 32;
        r = (r + dg + (more >> 4) - 8) & 255;
        g = (g + dg) & 255;
        b = (b + dg + (more & 15) - 8) & 255;
      } else if (op >= STEP) {
        r = (r + ((op >> 4) & 3) - 2) & 255;
        g = (g + ((op >> 2) & 3) - 2) & 255;
        b = (b + (op & 3) - 2) & 255;
      } else {
        const pixel = seen[op]!;
        r = pixel >>> 24;
        g = (pixel >> 16) & 255;
        b = (pixel >> 8) & 255;
        a = pixel & 255;
      }
      if (op >= STEP) seen[slot(r, g, b, a)] = (r << 24) | (g << 16) | (b << 8) | a;
    }
    pixels[i] = r;
    pixels[i + 1] = g;
    pixels[i + 2] = b;
    pixels[i + 3] = a;
  }
  return { pixels, width, height };
}
