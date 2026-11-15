/**
 * The paints a picture is drawn in: one flat colour for each kind of face a block may show, named after the game's
 * own textures.
 */
import type { Biome } from './biomes';

let count = 0;

export const GRASS_TOP = count++;
/** The side of a grass block: soil with the tinted overlay laid over it. */
export const GRASS_SIDE = count++;
export const DIRT_TEX = count++;
export const PODZOL_TOP = count++;
export const PODZOL_SIDE = count++;
export const STONE_TEX = count++;
export const GRAVEL_TEX = count++;
export const SAND_TEX = count++;
export const SANDSTONE_TEX = count++;
export const RED_SAND_TEX = count++;
export const SNOW_TEX = count++;
export const SNOW_SIDE = count++;
export const ICE_TEX = count++;
export const CLAY_PLAIN = count++;
export const CLAY_ORANGE = count++;
export const CLAY_YELLOW = count++;
export const CLAY_BROWN = count++;
export const CLAY_RED = count++;
export const CLAY_WHITE = count++;
export const CLAY_GREY = count++;
export const OAK_SIDE = count++;
export const OAK_TOP = count++;
export const BIRCH_SIDE = count++;
export const BIRCH_TOP = count++;
export const SPRUCE_SIDE = count++;
export const SPRUCE_TOP = count++;
export const DARK_OAK_SIDE = count++;
export const DARK_OAK_TOP = count++;
export const ACACIA_SIDE = count++;
export const ACACIA_TOP = count++;
export const CHERRY_SIDE = count++;
export const CHERRY_TOP = count++;
export const OAK_LEAVES_TEX = count++;
export const BIRCH_LEAVES_TEX = count++;
export const SPRUCE_LEAVES_TEX = count++;
export const DARK_OAK_LEAVES_TEX = count++;
export const ACACIA_LEAVES_TEX = count++;
export const CHERRY_LEAVES_TEX = count++;
export const CACTUS_SIDE = count++;
export const CACTUS_TOP = count++;
const PAINTS = count;

/** A colour the game gives as sRGB, as light, at `share` of its strength. */
const tint = (srgb: number, share: number): [number, number, number] => [
  ((srgb >> 16) / 255) ** 2.2 * share,
  (((srgb >> 8) & 255) / 255) ** 2.2 * share,
  ((srgb & 255) / 255) ** 2.2 * share,
];

/** Every paint's colour in a biome, as light, three values each: its own greens, then what is the same everywhere. */
export function palette(biome: Biome): Float32Array {
  const all = new Float32Array(PAINTS * 3);
  const paint = (texture: number, r: number, g: number, b: number): void => all.set([r, g, b], texture * 3);
  const grass = tint(biome.grass, 0.5);
  const leaf = tint(biome.foliage, 0.42);
  paint(GRASS_TOP, ...grass);
  paint(GRASS_SIDE, grass[0] * 0.45 + 0.088, grass[1] * 0.45 + 0.058, grass[2] * 0.45 + 0.036);
  paint(OAK_LEAVES_TEX, ...leaf);
  paint(DARK_OAK_LEAVES_TEX, leaf[0] * 0.75, leaf[1] * 0.75, leaf[2] * 0.75);
  paint(ACACIA_LEAVES_TEX, ...leaf);
  paint(BIRCH_LEAVES_TEX, ...tint(0x80a755, 0.42));
  paint(SPRUCE_LEAVES_TEX, 0.04, 0.15, 0.11);
  paint(CHERRY_LEAVES_TEX, 0.75, 0.3, 0.45);
  // The ground.
  paint(DIRT_TEX, 0.16, 0.105, 0.065);
  paint(PODZOL_TOP, 0.2, 0.125, 0.06);
  paint(PODZOL_SIDE, 0.16, 0.105, 0.065);
  paint(STONE_TEX, 0.17, 0.18, 0.185);
  paint(GRAVEL_TEX, 0.21, 0.21, 0.215);
  paint(SAND_TEX, 0.5, 0.4, 0.24);
  paint(SANDSTONE_TEX, 0.4, 0.3, 0.17);
  paint(RED_SAND_TEX, 0.4, 0.17, 0.07);
  paint(SNOW_TEX, 0.56, 0.62, 0.68);
  paint(SNOW_SIDE, 0.4, 0.42, 0.44);
  paint(ICE_TEX, 0.3, 0.42, 0.56);
  paint(CLAY_PLAIN, 0.3, 0.14, 0.09);
  paint(CLAY_ORANGE, 0.38, 0.15, 0.05);
  paint(CLAY_YELLOW, 0.4, 0.28, 0.09);
  paint(CLAY_BROWN, 0.26, 0.085, 0.05);
  paint(CLAY_RED, 0.28, 0.07, 0.05);
  paint(CLAY_WHITE, 0.4, 0.35, 0.29);
  paint(CLAY_GREY, 0.24, 0.17, 0.15);
  paint(CACTUS_SIDE, 0.09, 0.22, 0.08);
  paint(CACTUS_TOP, 0.09, 0.22, 0.08);
  // Wood: each kind's bark, and its sawn planks.
  paint(OAK_SIDE, 0.13, 0.085, 0.05);
  paint(OAK_TOP, 0.3, 0.2, 0.1);
  paint(BIRCH_SIDE, 0.5, 0.47, 0.4);
  paint(BIRCH_TOP, 0.42, 0.34, 0.2);
  paint(SPRUCE_SIDE, 0.075, 0.05, 0.035);
  paint(SPRUCE_TOP, 0.2, 0.125, 0.065);
  paint(DARK_OAK_SIDE, 0.085, 0.06, 0.065);
  paint(DARK_OAK_TOP, 0.1, 0.06, 0.035);
  paint(ACACIA_SIDE, 0.19, 0.17, 0.15);
  paint(ACACIA_TOP, 0.42, 0.2, 0.09);
  paint(CHERRY_SIDE, 0.09, 0.05, 0.05);
  paint(CHERRY_TOP, 0.5, 0.3, 0.28);
  return all;
}
