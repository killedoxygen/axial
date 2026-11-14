import { clamp01, hash3, type Light } from './light';
import { valueNoise, type World } from './world';

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

/** The screen column being drawn: its direction over the ground, unnormalised, and what follows from it. */
export interface Ray {
  col: number;
  dirX: number;
  dirZ: number;
  /** The direction's length squared. */
  span: number;
  /** Its angle from straight ahead. */
  angle: number;
}

/** The colour of starlight: the night's blue, paled. */
const STAR = [0.26, 0.38, 0.52] as const;

/** The sky of one hour: its colour in every direction, then stars, the sun or moon, and clouds. */
export class Sky {
  /** Where open sky near the light is noted at quarter size, the source of the light shafts: the strip being drawn. */
  open = new Float32Array(0);
  openWidth = 0;
  openFrom = 0;
  /** Where the light stands from straight ahead, and whether it is in front of the camera at all. */
  readonly azimuth: number;
  readonly ahead: boolean;
  private readonly tap = new Float32Array(3);
  private readonly seed: number;
  /** The sun's and the moon's pictures as light, thirty-two texels a side, where the version has them. */
  private readonly sunBody: Float32Array | undefined;
  private readonly moonBody: Float32Array | undefined;
  /** The axes of the square they are drawn on: across it, and up it. */
  private readonly rightX: number;
  private readonly rightZ: number;
  private readonly overX: number;
  private readonly overY: number;
  private readonly overZ: number;
  /** How tightly the glare keeps to the light: wide at night, close at noon. */
  private readonly glare: number;

  constructor(
    world: World,
    private readonly light: Light,
    private readonly view: View,
    private readonly ray: Ray,
  ) {
    this.seed = world.seed;
    this.azimuth = Math.atan2(light.x, light.z) - view.yaw;
    this.ahead = light.x * Math.sin(view.yaw) + light.z * Math.cos(view.yaw) > 0;
    const [sunX, sunY, sunZ] = light.sun;
    const flat = Math.hypot(sunX, sunZ) || 1;
    this.rightX = sunZ / flat;
    this.rightZ = -sunX / flat;
    this.overX = sunY * this.rightZ;
    this.overY = sunZ * this.rightX - sunX * this.rightZ;
    this.overZ = -sunY * this.rightX;
    const falloff = 0.5 + (0.25 - 0.15 * light.noon - 0.5) * light.daylight;
    this.glare = falloff * 0.1;
    // Packs brighten both; the moon they drain to grey and tint with the night's own blue.
    const lit = (texture: Uint8Array | undefined, moon: boolean): Float32Array | undefined => {
      if (!texture) return undefined;
      const body = new Float32Array(1024 * 3);
      for (let i = 0; i < 1024; i++) {
        const texel = [0, 1, 2].map((c) => (texture[i * 4 + c]! / 255) ** 2.2 * (texture[i * 4 + 3]! / 255) * 2.25);
        const grey = (texel[0]! * 0.2126 + texel[1]! * 0.7152 + texel[2]! * 0.0722) * 4;
        for (let c = 0; c < 3; c++) body[i * 3 + c] = moon ? grey * Math.sqrt(light.night[c]!) ** 1.6 : texel[c]!;
      }
      return body;
    };
    this.sunBody = lit(world.sunTexture, false);
    this.moonBody = lit(world.moonTexture, true);
  }

  private cloud(x: number, z: number): number {
    return (
      valueNoise(x, z * 1.25, this.seed + 71) * 0.56 +
      valueNoise(x * 2.3 + 5, z * 2.9, this.seed + 73) * 0.29 +
      valueNoise(x * 5.3, z * 6.1 + 3, this.seed + 75) * 0.15
    );
  }

  /**
   * The sky's own colour in a direction, without sun, moon or cloud: `up` is how steeply the direction climbs and
   * `toSun` how nearly it points at the sun, both as cosines. Blue thinning toward the horizon by day, the sun's
   * colour spreading along the horizon when it is low, a deep blue at night.
   */
  color(out: Float32Array, up: number, toSun: number): void {
    const { daylight, noon, skyBlue, sunSky, night, skyExposure } = this.light;
    const lift = up > 0 ? up : 0;
    const gradient = Math.exp(-(1 - (1 - lift) ** (1.5 - 0.5 * toSun)) / 0.35);
    const sunward = ((toSun * 0.5 + 0.5) * (1 - lift)) ** (2 - daylight) * (1 - noon * 0.6) ** 3;
    const mix = 1 - (1 - sunward) * (1 - (1 - lift) ** 2.5 * 0.125);
    const deep = (0.25 + 0.75 * Math.exp(-lift / 0.3)) * 0.42;
    for (let c = 0; c < 3; c++) {
      const blue = skyBlue[c]! * gradient;
      const day = (blue / Math.sqrt(blue * blue + 1)) * skyExposure * daylight;
      const both = Math.sqrt(day * (1 - mix)) * (1 - mix) + Math.sqrt(sunSky[c]! * gradient) * mix;
      const dark = night[c]! * deep;
      out[c] = dark + (both * both - dark) * daylight * daylight;
    }
  }

  /** The whole sky at a row of the ray's column, written at `o` in `out`. `record` notes open sky for the shafts. */
  into(out: Float32Array, o: number, row: number, record: boolean): void {
    const { light, view, ray, tap, seed } = this;
    const { daylight } = light;
    const { focal } = view;
    const { col, dirX, dirZ, angle } = ray;
    const v = (view.horizon - row - 0.5) / focal;
    const length = Math.sqrt(ray.span + v * v);
    const up = Math.max(0, v) / length;
    const toSun = (dirX * light.sun[0] + Math.max(0, v) * light.sun[1] + dirZ * light.sun[2]) / length;
    this.color(tap, up, toSun);
    let r = tap[0]!;
    let g = tap[1]!;
    let b = tap[2]!;
    const da = angle - this.azimuth;
    const dv = Math.max(0, v) - light.rise;
    const halo = this.ahead ? Math.exp(-(da * da * 1.4 + dv * dv * 6)) : 0;
    const nightfall = clamp01(1 - daylight * 1.5);
    if (nightfall > 0 && v > 0) {
      // Stars as shader packs scatter them: the sky folded flat into a fine grid, and a cell in a thousand lit.
      const flat = Math.sqrt(ray.span);
      const fold = 409.6 / (v + flat);
      const sx = Math.floor(dirX * fold);
      const sz = Math.floor(dirZ * fold);
      const star = hash3(sx, sz, seed) * hash3(sx, sz, seed + 1) * hash3(sx, sz, seed + 2) - 0.78;
      // None show through the moon's own glare.
      if (star > 0 && toSun > -0.992) {
        const k = star * 5 * Math.sqrt(Math.sqrt(up)) * nightfall;
        r += k * STAR[0];
        g += k * STAR[1];
        b += k * STAR[2];
      }
    }
    // The sun and the moon are the game's own pictures of them, far brighter than white so that they bloom,
    // each on a square that faces the camera from its place in the sky.
    const side = toSun > 0 ? 1 : -1;
    const body = side > 0 ? this.sunBody : this.moonBody;
    const along = (dirX * light.sun[0] + v * light.sun[1] + dirZ * light.sun[2]) * side;
    if (body && along > 0 && v > 0) {
      const reach = (side > 0 ? 0.3 : 0.2) * along;
      const across = dirX * this.rightX + dirZ * this.rightZ;
      const over = dirX * this.overX + v * this.overY + dirZ * this.overZ;
      if (Math.abs(across) < reach && Math.abs(over) < reach) {
        const at = (Math.floor((1 - over / reach) * 16) * 32 + Math.floor((across / reach + 1) * 16)) * 3;
        const fade = (1 - (1 - (up * 0.975 + 0.025)) ** 8) ** 2 * (side > 0 ? daylight : 1 - daylight);
        r += body[at]! * fade;
        g += body[at + 1]! * fade;
        b += body[at + 2]! * fade;
      }
    }
    // A wide glare round whichever of them lights the night or the day's end.
    const toLight = light.sunUp ? toSun : -toSun;
    const glare = (this.glare / (1 - (1 - this.glare) * (toLight * 0.5 + 0.5)) - this.glare) * 0.25 * (1 - light.noon);
    r += light.color[0] * glare;
    g += light.color[1] * glare;
    b += light.color[2] * glare;
    // Clouds on a plane overhead: lit by the light on the side toward it and at their rims, by the sky in their
    // thick middles.
    const d = 70 / v;
    if (v > 0.015 && d < 1800) {
      const wx = (view.x + dirX * d) / 130;
      const wz = (view.z + dirZ * d) / 130;
      const puff = this.cloud(wx, wz);
      if (puff > 0.52) {
        const cover =
          Math.min(1, (puff - 0.52) * 7) * Math.min(1, v * 10) * Math.max(0, 1 - d / 1800) * (0.35 + 0.65 * daylight);
        const thick = this.cloud(wx + light.towardX * 0.1, wz + light.towardZ * 0.1);
        const glare = ((light.sunUp ? toSun : -toSun) * 0.5 + 0.5) ** 6;
        const bright = clamp01(1.15 - (thick - 0.47) * 3.4) * (1 - Math.min(1, (puff - 0.52) * 4.5) * 0.5);
        const shown = bright + (1 - bright) * (1 - cover * cover) * glare * 0.5;
        const dim = 0.5 - 0.25 * (1 - daylight);
        const lay = (sky: number, c: number): number => {
          const base = light.ambient[c]! * (0.3 * daylight + 0.5);
          const cloud = (base + (light.color[c]! * (0.85 + 1.15 * glare) - base) * shown) * dim;
          return sky + (cloud - sky) * cover * cover;
        };
        r = lay(r, 0);
        g = lay(g, 1);
        b = lay(b, 2);
      }
    }
    if (record && row >= 0 && row < view.height)
      this.open[(row >> 2) * this.openWidth + (col >> 2) - this.openFrom] += halo / 16;
    out[o] = r;
    out[o + 1] = g;
    out[o + 2] = b;
  }
}
