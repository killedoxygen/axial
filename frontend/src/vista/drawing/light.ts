import { FOLIAGE, GLOWS, LAMP, LAVA, MAGMA, MATERIALS, RUNS, SEA_PICKLES, WINDOW } from './blocks';
import { GRID } from './land';
import type { World } from './world';

export type Rgb = readonly [number, number, number];

export const clamp01 = (value: number): number => (value < 0 ? 0 : value > 1 ? 1 : value);

export function hash3(x: number, y: number, z: number): number {
  let h = Math.imul(x, 0x27d4eb2d) ^ Math.imul(y, 0x165667b1) ^ Math.imul(z, 0x9e3779b9);
  h = Math.imul(h ^ (h >>> 15), 0x85ebca6b);
  h = Math.imul(h ^ (h >>> 13), 0xc2b2ae35);
  return ((h ^ (h >>> 16)) >>> 0) / 4294967296;
}

/** A colour at a height in the sky. */
export type Stop = readonly [number, Rgb];

const scaled = (color: Rgb, by: number): Rgb => [color[0] * by, color[1] * by, color[2] * by];

/** The sky straight overhead: nearly the panel's own dark. */
const ZENITH: Rgb = [0.0065, 0.0091, 0.007];
/** How much brighter than its colour what gives light shows. */
const GLOW = 4.5;

/**
 * The light of a world's evening, as a few colours. The last light is low ahead and warm, and casts long shadows;
 * a cool one from behind the camera fills the near sides; the air between here and far things is one colour.
 */
export class Light {
  /** Unit vector toward the last light. */
  readonly x: number;
  readonly y: number;
  readonly z: number;
  /** What a face turned to the last light takes from it, and what every face takes from the open sky. */
  readonly key: Rgb;
  readonly fill: Rgb;
  /** The light from behind the camera: toward it, and its colour. */
  readonly back: Rgb;
  readonly backColor: Rgb;
  /** What a point of light gives the land round it. */
  readonly lamp: Rgb = [1.0, 0.56, 0.24];
  /** The air: how far off it begins and is at its most, how much it then hides, and its colour. */
  readonly haze: { from: number; to: number; most: number; color: Rgb };
  /**
   * The sky as two runs of colour by height, each a list of [how steeply the direction climbs, colour]: the one
   * toward the light and the one away from it.
   */
  readonly ramps: { warm: Stop[]; cool: Stop[] };
  /** The water's own colour, under what it mirrors. */
  readonly water: Rgb = [0.004, 0.012, 0.014];
  /** What each material that gives light shows of itself: three values a material. */
  readonly glows = new Float32Array(MATERIALS * 3);

  constructor({ light, dusk }: World) {
    [this.x, this.y, this.z] = light;
    this.key = scaled(dusk.glow, 4 / Math.PI);
    this.fill = scaled(dusk.cool, 0.32);
    const turn = Math.hypot(this.x, this.z) || 1;
    this.back = [(-this.x / turn) * 0.788, 0.616, (-this.z / turn) * 0.788];
    this.backColor = scaled(dusk.fill, 0.95 / Math.PI);
    this.haze = { from: 55, to: 210, most: 0.93, color: dusk.haze };
    this.ramps = {
      warm: [
        [0, scaled(dusk.glow, 1.25)],
        [0.06, [0.9, 0.4, 0.2]],
        [0.14, [0.22, 0.1, 0.14]],
        [0.22, [0.02, 0.035, 0.045]],
        [0.32, ZENITH],
      ],
      cool: [
        [0, scaled(dusk.cool, 0.5)],
        [0.28, ZENITH],
      ],
    };
    const gives = (mat: number, color: Rgb): void => this.glows.set(scaled(color, GLOW * GLOWS[mat]!), mat * 3);
    gives(LAMP, [0.55, 0.26, 0.07]);
    gives(WINDOW, [1.0, 0.5, 0.17]);
    gives(LAVA, [0.5, 0.12, 0.02]);
    gives(MAGMA, [0.55, 0.075, 0.008]);
    gives(SEA_PICKLES, [0.22, 0.42, 0.26]);
  }
}

/**
 * What stands in each column of the world: the highest solid block, the highest foliage, and the highest solid
 * block among its eight neighbours, which is all the corner shading needs to know to skip open ground.
 */
export class Tops {
  readonly solid = new Uint8Array(GRID * GRID);
  readonly leaf = new Uint8Array(GRID * GRID);
  readonly rim = new Uint8Array(GRID * GRID);

  constructor({ ground, runs, runCount }: World) {
    const n = GRID;
    const { solid, leaf, rim } = this;
    for (let i = 0; i < n * n; i++) {
      solid[i] = ground[i]!;
      for (let k = 0; k < runCount[i]!; k++) {
        const ro = (i * RUNS + k) * 3;
        const mat = runs[ro + 2]!;
        if (mat >= FOLIAGE && mat < LAMP) leaf[i] = runs[ro + 1]!;
        else solid[i] = Math.max(solid[i]!, runs[ro + 1]!);
      }
    }
    const around = [-1, 1, -n, n, -n - 1, -n + 1, n - 1, n + 1];
    for (let z = 1; z < n - 1; z++) {
      for (let x = 1; x < n - 1; x++) {
        const i = z * n + x;
        for (const step of around) if (solid[i + step]! > rim[i]!) rim[i] = solid[i + step]!;
      }
    }
  }
}
