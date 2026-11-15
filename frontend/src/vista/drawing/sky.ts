import { clamp01, type Light, type Stop } from './light';

/** Where the picture is taken from, and how the world lands on it. */
export interface View {
  readonly width: number;
  readonly height: number;
  /** Pixels across one unit of slope from straight ahead. */
  readonly focal: number;
  /** The row the horizon lies on. */
  readonly horizon: number;
  readonly x: number;
  readonly y: number;
  readonly z: number;
  readonly yaw: number;
}

/** The screen column being drawn: its direction over the ground, unnormalised, and that direction's length squared. */
export interface Ray {
  dirX: number;
  dirZ: number;
  span: number;
}

/** One channel of a run of colours at a height. */
function along(stops: Stop[], height: number, channel: number): number {
  let at = 1;
  while (at < stops.length - 1 && stops[at]![0] < height) at++;
  const [low, from] = stops[at - 1]!;
  const [high, to] = stops[at]!;
  const t = clamp01((height - low) / (high - low));
  return from[channel]! + (to[channel]! - from[channel]!) * t;
}

/**
 * The sky of a world's evening: the warm run of colours toward the last light, the cool one away from it, and
 * each giving way to the other across the picture.
 */
export class Sky {
  private readonly tap = new Float32Array(3);

  constructor(
    private readonly light: Light,
    private readonly view: View,
    private readonly ray: Ray,
  ) {}

  /**
   * The sky's colour in a direction: `up` is how steeply the direction climbs and `toLight` how nearly it points
   * at the light, both as cosines.
   */
  color(out: Float32Array, up: number, toLight: number): void {
    const { ramps } = this.light;
    const lift = up > 0 ? up : 0;
    const t = clamp01((toLight - 0.15) / 0.83);
    const toward = t * t * (3 - 2 * t);
    for (let c = 0; c < 3; c++) {
      const cool = along(ramps.cool, lift, c);
      out[c] = cool + (along(ramps.warm, lift, c) - cool) * toward;
    }
  }

  /** The sky at a row of the ray's column, written at `o` in `out`. */
  into(out: Float32Array, o: number, row: number): void {
    const { light, view, ray, tap } = this;
    const v = Math.max(0, (view.horizon - row - 0.5) / view.focal);
    const length = Math.sqrt(ray.span + v * v);
    this.color(tap, v / length, (ray.dirX * light.x + v * light.y + ray.dirZ * light.z) / length);
    out[o] = tap[0]!;
    out[o + 1] = tap[1]!;
    out[o + 2] = tap[2]!;
  }
}
