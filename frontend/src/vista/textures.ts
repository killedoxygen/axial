/**
 * The block textures a picture is drawn with: an instance's own, by the names the game gives them. Grass and foliage
 * arrive grey and take their colour from the biome, as in the game. Every texture gets the smaller copies that keep
 * distant ground from turning to noise.
 */

/** A game's textures by name: sixteen rows of sixteen RGBA texels each. */
export type TexturePack = ReadonlyMap<string, Uint8Array>;

const NAMES: string[] = [];
const texture = (name: string): number => NAMES.push(name) - 1;

export const GRASS_TOP = texture('grass_block_top');
/** The side of a grass block: soil with the tinted overlay laid over it. */
export const GRASS_SIDE = texture('grass_block_side');
export const DIRT_TEX = texture('dirt');
export const PODZOL_TOP = texture('podzol_top');
export const PODZOL_SIDE = texture('podzol_side');
export const STONE_TEX = texture('stone');
export const ANDESITE_TEX = texture('andesite');
export const GRANITE_TEX = texture('granite');
export const COAL_TEX = texture('coal_ore');
export const GRAVEL_TEX = texture('gravel');
export const SAND_TEX = texture('sand');
export const SANDSTONE_TEX = texture('sandstone');
export const RED_SAND_TEX = texture('red_sand');
export const SNOW_TEX = texture('snow');
export const SNOW_SIDE = texture('grass_block_snow');
export const ICE_TEX = texture('ice');
export const OBSIDIAN_TEX = texture('obsidian');
export const CRYING_TEX = texture('crying_obsidian');
export const NETHERRACK_TEX = texture('netherrack');
export const MAGMA_TEX = texture('magma');
export const CLAY_PLAIN = texture('terracotta');
export const CLAY_ORANGE = texture('orange_terracotta');
export const CLAY_YELLOW = texture('yellow_terracotta');
export const CLAY_BROWN = texture('brown_terracotta');
export const CLAY_RED = texture('red_terracotta');
export const CLAY_WHITE = texture('white_terracotta');
export const CLAY_GREY = texture('light_gray_terracotta');
export const LAVA_TEX = texture('lava_still');
export const OAK_SIDE = texture('oak_log');
export const OAK_TOP = texture('oak_log_top');
export const BIRCH_SIDE = texture('birch_log');
export const BIRCH_TOP = texture('birch_log_top');
export const SPRUCE_SIDE = texture('spruce_log');
export const SPRUCE_TOP = texture('spruce_log_top');
export const DARK_OAK_SIDE = texture('dark_oak_log');
export const DARK_OAK_TOP = texture('dark_oak_log_top');
export const ACACIA_SIDE = texture('acacia_log');
export const ACACIA_TOP = texture('acacia_log_top');
export const CHERRY_SIDE = texture('cherry_log');
export const CHERRY_TOP = texture('cherry_log_top');
export const OAK_LEAVES_TEX = texture('oak_leaves');
export const BIRCH_LEAVES_TEX = texture('birch_leaves');
export const SPRUCE_LEAVES_TEX = texture('spruce_leaves');
export const DARK_OAK_LEAVES_TEX = texture('dark_oak_leaves');
export const ACACIA_LEAVES_TEX = texture('acacia_leaves');
export const CHERRY_LEAVES_TEX = texture('cherry_leaves');
export const VINE_TEX = texture('vine');
export const CACTUS_SIDE = texture('cactus_side');
export const CACTUS_TOP = texture('cactus_top');
export const SHORT_GRASS_TEX = texture('short_grass');
export const TALL_GRASS_LOW = texture('tall_grass_bottom');
export const TALL_GRASS_HIGH = texture('tall_grass_top');
export const FERN_TEX = texture('fern');
export const POPPY_TEX = texture('poppy');
export const DANDELION_TEX = texture('dandelion');
export const CORNFLOWER_TEX = texture('cornflower');
export const DAISY_TEX = texture('oxeye_daisy');
export const BLUET_TEX = texture('azure_bluet');
export const ALLIUM_TEX = texture('allium');
export const ORCHID_TEX = texture('blue_orchid');
export const DEAD_BUSH_TEX = texture('dead_bush');
export const CANE_TEX = texture('sugar_cane');
export const RED_CAP_TEX = texture('red_mushroom');
export const BROWN_CAP_TEX = texture('brown_mushroom');
export const LILY_TEX = texture('lily_pad');
export const PETALS_TEX = texture('pink_petals');
export const TEXTURES = NAMES.length;

/** Texels before each mip level of one texture: 16, 8, 4, 2, 1 across. */
export const MIP_OFFSET = [0, 256, 320, 336, 340] as const;
export const TEXELS = 341;

/** Which biome colour a grey texture takes. Birch, spruce and lily pads keep one green everywhere, as in the game. */
const TINTS: Partial<Record<number, 'grass' | 'foliage' | number>> = {
  [GRASS_TOP]: 'grass',
  [SHORT_GRASS_TEX]: 'grass',
  [TALL_GRASS_LOW]: 'grass',
  [TALL_GRASS_HIGH]: 'grass',
  [FERN_TEX]: 'grass',
  [CANE_TEX]: 'grass',
  [OAK_LEAVES_TEX]: 'foliage',
  [DARK_OAK_LEAVES_TEX]: 'foliage',
  [ACACIA_LEAVES_TEX]: 'foliage',
  [VINE_TEX]: 'foliage',
  [BIRCH_LEAVES_TEX]: 0x80a755,
  [SPRUCE_LEAVES_TEX]: 0x619961,
  [LILY_TEX]: 0x208030,
};

/** Textures that give off light, by how bright each texel is. */
const GLOWING = new Set([LAVA_TEX, MAGMA_TEX]);

/** Whether a game has the given texture. A version older than a block simply lacks it. */
export function hasTexture(pack: TexturePack, id: number): boolean {
  return pack.has(NAMES[id]!);
}

export interface Atlas {
  /** Linear RGB plus coverage for every texel of every mip level. */
  colors: Float32Array;
  /** How strongly each texel of every mip level glows; zero for all but lava and the like. */
  glow: Float32Array;
}

/** Shader packs read a texel as display colour to the power 2.2. */
const LINEAR = new Float32Array(256);
for (let i = 0; i < 256; i++) LINEAR[i] = (i / 255) ** 2.2;

/**
 * How much a texel shines of its own accord, from its colour alone: vivid bright texels and near-white ones glow,
 * the rest hardly at all. This is the rule shader packs apply to lava.
 */
function emission(r: number, g: number, b: number): number {
  const value = Math.max(r, g, b) / 255;
  const saturation = value > 0 ? 1 - Math.min(r, g, b) / 255 / value : 0;
  const vivid = Math.min(1, Math.max(0, saturation * 3.125 - 0.125)) * value ** 4;
  return Math.max(vivid, Math.min(1, Math.max(0, value * 7 - 6))) * 0.5;
}

/** Builds every texture from a game's own, for a biome whose grass and foliage have the given sRGB colours. */
export function buildAtlas(pack: TexturePack, grass: number, foliage: number): Atlas {
  const colors = new Float32Array(TEXTURES * TEXELS * 4);
  const glow = new Float32Array(TEXTURES * TEXELS);
  const overlay = pack.get('grass_block_side_overlay');
  for (let t = 0; t < TEXTURES; t++) {
    const source = pack.get(NAMES[t]!);
    if (!source) continue;
    const base = t * TEXELS;
    const rule = TINTS[t];
    const tint = rule === 'grass' ? grass : rule === 'foliage' ? foliage : rule;
    for (let i = 0; i < 256; i++) {
      let r = source[i * 4]!;
      let g = source[i * 4 + 1]!;
      let b = source[i * 4 + 2]!;
      let shade = tint;
      if (t === GRASS_SIDE && overlay && overlay[i * 4 + 3]! > 25) {
        r = overlay[i * 4]!;
        g = overlay[i * 4 + 1]!;
        b = overlay[i * 4 + 2]!;
        shade = grass;
      } else if (source[i * 4 + 3]! <= 25) continue;
      // The game multiplies the texel by the biome's colour before anything else sees it.
      if (shade !== undefined) {
        r = Math.round((r * (shade >> 16)) / 255);
        g = Math.round((g * ((shade >> 8) & 255)) / 255);
        b = Math.round((b * (shade & 255)) / 255);
      }
      const o = (base + i) * 4;
      colors[o] = LINEAR[r]!;
      colors[o + 1] = LINEAR[g]!;
      colors[o + 2] = LINEAR[b]!;
      colors[o + 3] = 1;
      if (GLOWING.has(t)) glow[base + i] = emission(r, g, b);
    }
    for (let mip = 1; mip < 5; mip++) {
      const size = 16 >> mip;
      const from = base + MIP_OFFSET[mip - 1]!;
      const to = base + MIP_OFFSET[mip]!;
      for (let y = 0; y < size; y++) {
        for (let x = 0; x < size; x++) {
          let r = 0;
          let g = 0;
          let b = 0;
          let a = 0;
          let shine = 0;
          for (let k = 0; k < 4; k++) {
            const s = from + (y * 2 + (k >> 1)) * size * 2 + x * 2 + (k & 1);
            const cover = colors[s * 4 + 3]!;
            r += colors[s * 4]! * cover;
            g += colors[s * 4 + 1]! * cover;
            b += colors[s * 4 + 2]! * cover;
            a += cover;
            shine += glow[s]!;
          }
          const o = (to + y * size + x) * 4;
          if (a > 0) {
            colors[o] = r / a;
            colors[o + 1] = g / a;
            colors[o + 2] = b / a;
          }
          colors[o + 3] = a / 4;
          glow[to + y * size + x] = shine / 4;
        }
      }
    }
  }
  return { colors, glow };
}
