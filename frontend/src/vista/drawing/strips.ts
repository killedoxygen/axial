/** How many quarter-size cells cover `pixels`. */
export const quarter = (pixels: number): number => Math.ceil(pixels / 4);

/** What columns `from` up to `to` give the whole picture's bloom (`soft`) and light shafts (`open`), at quarter size. */
export interface Quarter {
  from: number;
  to: number;
  soft: Float32Array;
  open: Float32Array;
}

/** Columns `from` up to `to` of the finished picture. */
export interface Strip {
  from: number;
  to: number;
  pixels: Uint8ClampedArray<ArrayBuffer>;
}

/** Copies `part`, columns `from` up to `to` of every row, into `whole`, which is `width` across; `cells` values a pixel. */
function place(
  whole: Float32Array | Uint8ClampedArray,
  width: number,
  part: Float32Array | Uint8ClampedArray,
  from: number,
  to: number,
  cells: number,
): void {
  const run = (to - from) * cells;
  for (let row = 0, at = 0; at < part.length; row++, at += run) {
    whole.set(part.subarray(at, at + run), (row * width + from) * cells);
  }
}

/** A picture coming together from strips drawn apart. */
export class Sheet {
  readonly soft: Float32Array;
  readonly open: Float32Array;
  readonly pixels: Uint8ClampedArray<ArrayBuffer>;

  constructor(
    readonly width: number,
    readonly height: number,
  ) {
    const cells = quarter(width) * quarter(height);
    this.soft = new Float32Array(cells * 3);
    this.open = new Float32Array(cells);
    this.pixels = new Uint8ClampedArray(width * height * 4);
  }

  lay({ from, to, soft, open }: Quarter): void {
    place(this.soft, quarter(this.width), soft, from >> 2, quarter(to), 3);
    place(this.open, quarter(this.width), open, from >> 2, quarter(to), 1);
  }

  set({ from, to, pixels }: Strip): void {
    place(this.pixels, this.width, pixels, from, to, 4);
  }
}
