/**
 * Block textures painted in code to the game's conventions: a small palette in fixed proportions, grass and foliage
 * in grey to be tinted, cut-outs with real gaps. They stand in for a game's own files where there is no game.
 */

function hash(x: number, y: number, salt: number): number {
  let h = Math.imul(x & 15, 0x27d4eb2d) ^ Math.imul(y & 15, 0x165667b1) ^ Math.imul(salt, 0x9e3779b9);
  h = Math.imul(h ^ (h >>> 15), 0x85ebca6b);
  h = Math.imul(h ^ (h >>> 13), 0xc2b2ae35);
  return ((h ^ (h >>> 16)) >>> 0) / 4294967296;
}

/** A texel: an sRGB colour, or -1 for see-through. */
type Painter = (x: number, y: number) => number;

/** A palette from dark to light with the share of the texture each tone takes. */
type Tones = readonly (readonly [color: number, share: number])[];

/** Picks the tone a value in 0..1 falls on, so a texture uses its palette in the stated proportions. */
function tone(tones: Tones, value: number): number {
  let total = 0;
  for (const [, share] of tones) total += share;
  let at = value * total;
  for (const [color, share] of tones) {
    if (at < share) return color;
    at -= share;
  }
  return tones[tones.length - 1]![0];
}

/** Grain: how neighbouring texels cluster. `across` and `down` stretch the clusters; `slant` shears them. */
const grain =
  (tones: Tones, salt: number, across = 0, down = 0, slant = 0, fine = 0.5): Painter =>
  (x, y) =>
    tone(tones, hash((x + ((y * slant) | 0)) >> across, y >> down, salt) * (1 - fine) + hash(x, y, salt + 1) * fine);

/** Streaks: every line of texels along the grain is a row of strokes two to five long, each of one tone. */
const streaks =
  (tones: Tones, salt: number, upright = true): Painter =>
  (x, y) => {
    const line = upright ? x : y;
    const along = upright ? y : x;
    let end = -Math.floor(hash(line, 0, salt) * 4);
    let stroke = 0;
    do end += 2 + Math.floor(hash(line, ++stroke, salt + 1) * 4);
    while (end <= along);
    return tone(tones, hash(line, stroke, salt + 2));
  };

/** Pixel art written out: rows of palette digits, `.` for a gap, standing on the bottom edge of the texture. */
const drawn = (palette: readonly number[], rows: readonly string[]): Painter => {
  const lift = 16 - rows.length;
  return (x, y) => {
    const mark = y < lift ? '.' : (rows[y - lift]![x] ?? '.');
    return mark === '.' ? -1 : palette[Number(mark)]!;
  };
};

const SOIL: Tones = [
  [0x593d29, 13],
  [0x79553a, 42],
  [0x966c4a, 28],
  [0xb9855c, 15],
];
const soil = grain(SOIL, 1, 1, 0, 1, 0.55);
const dirt: Painter = (x, y) => (hash(x, y, 3) > 0.972 ? (hash(x, y, 4) > 0.5 ? 0x878787 : 0x6c6c6c) : soil(x, y));

/** Grey, to be tinted: fine and even, a touch streaky. */
const turf = grain(
  [
    [0x7f7f7f, 20],
    [0x888888, 30],
    [0x8f8f8f, 26],
    [0x979797, 24],
  ],
  5,
  0,
  1,
  0,
  0.7,
);

/** Soil with the surface hanging over it: a full top row, then drips of uneven length. */
const fringed =
  (surface: Painter, shadow: number, salt: number): Painter =>
  (x, y) => {
    const drip = 1 + Math.floor(hash(x, 0, salt) * 3) + (hash(x >> 1, 1, salt) > 0.75 ? 1 : 0);
    if (y >= drip) return dirt(x, y);
    return y === drip - 1 && drip > 1 ? shadow : surface(x, y);
  };

const litter = grain(
  [
    [0x4a3018, 22],
    [0x523c18, 37],
    [0x524c10, 7],
    [0x6a4418, 26],
    [0x8b5920, 6],
    [0xac6520, 2],
  ],
  6,
  1,
  0,
  1,
  0.6,
);

const stone = grain(
  [
    [0x686868, 7],
    [0x747474, 28],
    [0x7f7f7f, 46],
    [0x8f8f8f, 19],
  ],
  7,
  2,
  0,
  1,
  0.35,
);

/** Stone with dark lumps of ore set in it, each with a lighter lip under it. */
const ore: Painter = (x, y) => {
  const cx = (x + (y >> 2) * 6) & 15;
  const lump = hash(cx >> 2, y >> 2, 66) > 0.5;
  const lx = cx & 3;
  const ly = y & 3;
  if (lump && lx > 0 && ly > 0 && ly < 3 && (lx < 3 || hash(cx, y, 67) > 0.5))
    return [0x252525, 0x2e2e2e, 0x363636, 0x393c36][Math.floor(hash(x, y, 68) * 4)]!;
  if (lump && ly === 3 && lx > 0) return 0x5c5c5c;
  return stone(x, y);
};

const sandy = (tones: readonly number[], salt: number): Painter =>
  grain(
    [
      [tones[0]!, 4],
      [tones[1]!, 26],
      [tones[2]!, 42],
      [tones[3]!, 23],
      [tones[4]!, 5],
    ],
    salt,
    1,
    0,
    1,
    0.6,
  );

const sandstoneBody = grain(
  [
    [0xc6ae71, 12],
    [0xd1ba8a, 18],
    [0xd5c496, 8],
    [0xdad2a3, 28],
    [0xe3dbb0, 16],
    [0xe7e4bb, 18],
  ],
  14,
  2,
  0,
  0,
  0.45,
);
/** A pale cap, a dark seam under it, then the weathered body. */
const sandstone: Painter = (x, y) =>
  y === 0
    ? hash(x, 0, 12) > 0.4
      ? 0xedebcb
      : 0xe7e4bb
    : y === 1
      ? hash(x >> 1, 1, 12) > 0.5
        ? 0xdad2a3
        : 0xd5c496
      : y === 2
        ? hash(x >> 1, 2, 12) > 0.6
          ? 0xd1ba8a
          : 0xc6ae71
        : sandstoneBody(x, y);

/** Bark: upright streaks with dark flecks. Every kind of tree shares the pattern and differs in its six tones. */
const bark = (tones: readonly number[]): Painter => {
  const body = streaks(
    [
      [tones[1]!, 14],
      [tones[2]!, 15],
      [tones[3]!, 42],
      [tones[4]!, 17],
      [tones[5]!, 5],
    ],
    21,
  );
  return (x, y) => (hash(x, y, 24) > 0.93 ? tones[0]! : body(x, y));
};

/** A log's end: bark round the edge, then growth rings in four tones of the wood, from the ring line to the palest. */
const rings =
  (rind: readonly [number, number], wood: readonly number[]): Painter =>
  (x, y) => {
    const ring = Math.floor(Math.max(Math.abs(x - 7.5), Math.abs(y - 7.5)));
    if (ring === 7) return hash(x, y, 30) > 0.3 ? rind[1] : rind[0];
    if (ring === 5 || ring === 2 || ring === 0) return hash(x, y, 31) > 0.3 ? wood[0]! : wood[1]!;
    return hash(x, y, 32) > 0.3 ? wood[2]! : wood[3]!;
  };

const birchPale = streaks(
  [
    [0xbebeae, 4],
    [0xd8d8ce, 26],
    [0xded3d5, 12],
    [0xf0eeeb, 28],
    [0xffffff, 30],
  ],
  42,
  false,
);
/** White bark in level streaks, crossed by dark scars that fade at one end. */
const birch: Painter = (x, y) => {
  const row = (y * 5) & 15;
  const start = Math.floor(hash(row, 0, 41) * 16);
  const along = (x - start) & 15;
  const scar = hash(row, 1, 41) > 0.45 ? 3 + Math.floor(hash(row, 2, 41) * 3) : 0;
  if (along < scar) return along === scar - 1 ? 0xbebeae : along === scar - 2 ? 0x605e54 : 0x36342a;
  return birchPale(x, y);
};

/**
 * Foliage: small leaves set on a slanted lattice, each dark at its upper edge and light at its lower, with gaps
 * between. A crown therefore reads as layers of leaves and never as a flat sheet.
 * `tones` runs dark to light; `size` is how much of its lattice cell a leaf fills.
 */
const foliage =
  (tones: readonly number[], salt: number, size = 0.8): Painter =>
  (x, y) => {
    // The lattice steps (4, 2) and (-2, 3): sixteen texels a cell, and it closes over the texture's edges.
    const m = (3 * x + 2 * y) / 16;
    const n = (4 * y - 2 * x) / 16;
    const cm = Math.floor(m);
    const cn = Math.floor(n);
    const ox = 4 * cm - 2 * cn;
    const oy = 2 * cm + 3 * cn;
    const wide = size - 0.22 * hash(ox, oy, salt);
    const tall = size + 0.1 - 0.22 * hash(ox, oy, salt + 1);
    if (m - cm >= wide || n - cn >= tall) return -1;
    const t = (m - cm) * 2 + (n - cn) * 2.4 + (hash(x, y, salt + 2) - 0.5) * 1.5;
    return tones[t < 0.9 ? 0 : t < 1.6 ? 1 : t < 2.5 ? 2 : 3]!;
  };

const cherryLeaves: Painter = (x, y) => {
  if (hash(x, y, 33) > 0.88 || hash((x + y) >> 1, (y - x + 16) >> 1, 34) > 0.93) return -1;
  if (hash(x >> 1, (y + x) >> 1, 35) > 0.9) return 0x84992d;
  return tone(
    [
      [0xe67db5, 10],
      [0xeb92c1, 14],
      [0xefa7cd, 12],
      [0xf7b9dc, 16],
      [0xfccbe7, 28],
      [0xf5daef, 20],
    ],
    hash(x >> 1, y >> 1, 36) * 0.55 + hash(x, y, 37) * 0.45,
  );
};

/** Ribbed green between dark edges, with pale spines. */
const cactusSide: Painter = (x, y) => {
  if (x === 0 || x === 15 || x === 5 || x === 10) return hash(x, y, 51) > 0.86 ? 0xbabf95 : 0x39581a;
  return tone(
    [
      [0x527d26, 34],
      [0x5b8c2b, 33],
      [0x649832, 33],
    ],
    hash(x, y >> 2, 52) * 0.7 + hash(x, y, 53) * 0.3,
  );
};
const cactusTop: Painter = (x, y) => {
  if (Math.max(Math.abs(x - 7.5), Math.abs(y - 7.5)) > 7) return 0x39581a;
  if (hash(x >> 1, y >> 1, 54) > 0.9) return 0xbabf95;
  return tone(
    [
      [0x426520, 25],
      [0x527d26, 30],
      [0x5b8c2b, 23],
      [0x649832, 22],
    ],
    hash(x >> 1, y >> 1, 55) * 0.6 + hash(x, y, 56) * 0.4,
  );
};

const pebbles: Painter = (x, y) => {
  const lump = hash((x + (y >> 1)) >> 1, (y + (x >> 2)) >> 1, 15);
  const speck = hash(x, y, 16);
  if (speck > 0.94) return 0xb0aeae;
  return tone(
    [
      [0x645b5b, 6],
      [0x726b69, 20],
      [0x817f7f, 36],
      [0x89817e, 20],
      [0x979797, 18],
    ],
    lump * 0.7 + speck * 0.3,
  );
};

/** Fired clay: almost one colour. */
const clay =
  (colors: readonly number[], salt: number): Painter =>
  (x, y) =>
    colors[Math.floor((hash(x >> 1, y >> 1, salt) * 0.6 + hash(x, y, salt + 1) * 0.4) * colors.length)]!;

const lava: Painter = (x, y) => {
  const blob = hash((x + (y >> 1)) >> 2, (y + (x >> 1)) >> 2, 61) * 0.6 + hash(x >> 1, y >> 1, 62) * 0.4;
  if (blob > 0.74) return hash(x, y, 63) > 0.45 ? 0xfcd548 : 0xf5b028;
  if (blob > 0.6) return 0xe88a1e;
  return tone(
    [
      [0xd04a0a, 30],
      [0xd2510d, 30],
      [0xd5570f, 22],
      [0xd96415, 18],
    ],
    hash(x, y, 64),
  );
};

/**
 * Grass, grey to be tinted: one-texel blades rising from a dense foot, each to its own height, stepping aside as
 * it climbs and paler toward its tip. `up` counts texels from the ground, so a plant may stand two textures tall.
 */
const blades =
  (salt: number, stems: number, low: number, high: number, foot: number, tones: readonly number[]) =>
  (x: number, up: number): number => {
    const last = tones.length - 1;
    if (up < foot) {
      if (up > 0 && hash(x, up, salt + 5) > 0.82) return -1;
      return tones[Math.floor(hash(x, up, salt + 6) * (up < 2 ? 2.5 : 3.5))]!;
    }
    for (let stem = 0; stem < stems; stem++) {
      const height = low + hash(stem, 1, salt) * (high - low);
      if (up >= height) continue;
      const lean = hash(stem, 2, salt);
      const step = 2 + Math.floor(hash(stem, 3, salt) * 3);
      const root = Math.round(((stem + 0.5) * 16) / stems + (hash(stem, 0, salt) - 0.5) * 3);
      const at = root + (lean < 0.35 ? -1 : lean > 0.65 ? 1 : 0) * Math.floor((up - foot) / step);
      if ((at & 15) !== x) continue;
      const light = 1 + (up / height) * (last - 1) + (hash(stem, up, salt + 4) - 0.5) * 2;
      return tones[light < 0 ? 0 : light > last ? last : Math.round(light)]!;
    }
    return -1;
  };

const SHORT_TONES = [0x6c6c6c, 0x767776, 0x838183, 0x919191, 0xa0a0a0, 0xb8b7b8];
const TALL_TONES = [0x676967, 0x737573, 0x838183, 0x949194, 0xacaaac];
const shortGrass = blades(71, 8, 5, 14, 3, SHORT_TONES);
const tallGrass = blades(73, 7, 19, 31, 5, TALL_TONES);

/** Four stalks, each a light and a dark texel wide, with a pale joint every eight texels and a leaf off some. */
const cane: Painter = (x, y) => {
  const stalk = x >> 2;
  const lx = x & 3;
  const joint = (y + stalk * 5) & 7;
  if (lx === 1) return joint === 0 ? 0xe5efda : joint === 1 ? 0x82a859 : 0xaadb74;
  if (lx === 2) return joint === 0 ? 0x96c166 : joint === 1 ? 0x698747 : 0x82a859;
  if (joint === 2 && lx === (stalk & 1 ? 3 : 0) && ((y >> 3) + stalk) & 1) return stalk & 1 ? 0x91ef32 : 0x75c128;
  return -1;
};

/** Grey, to be tinted: a round leaf lying flat, a wedge cut from it, darker at the rim and along its veins. */
const lily: Painter = (x, y) => {
  const dx = x - 7.5;
  const dy = y - 7.5;
  const d = Math.hypot(dx, dy);
  if (d > 6.7 || (dy < -1 && Math.abs(dx) < 1.2)) return -1;
  if (d > 5.6) return hash(x, y, 91) > 0.5 ? 0x5c5c5c : 0x686868;
  if (Math.abs(Math.abs(dx) - Math.abs(dy)) < 0.6 || Math.abs(dx) < 0.6) return 0x7f7f7f;
  return hash(x, y, 92) > 0.5 ? 0xa3a3a3 : 0x969696;
};

/** Fallen blossom: a few small flowers lying on the ground. */
const petals: Painter = (x, y) => {
  for (const [cx, cy] of [
    [3, 3],
    [11, 4],
    [5, 11],
    [12, 12],
  ] as const) {
    const dx = Math.abs(x - cx);
    const dy = Math.abs(y - cy);
    if (dx + dy === 0) return 0xe563a3;
    if (dx + dy === 1) return 0xfccbe7;
    if (dx === 1 && dy === 1) return 0xf7b9dc;
    if (dx + dy === 2 && hash(cx, cy, 93) > 0.4) return 0xf5daef;
  }
  return hash(x, y, 94) > 0.95 ? 0xf6a1d4 : -1;
};

const STEM = [0x177c04, 0x4a8f28, 0x55ab2d];

const PAINTERS: Record<string, Painter> = {};
PAINTERS.grass_block_top = turf;
PAINTERS.grass_block_side = dirt;
PAINTERS.grass_block_side_overlay = (x, y) =>
  fringed(turf, 0x6c6c6c, 11)(x, y) === dirt(x, y) ? -1 : fringed(turf, 0x6c6c6c, 11)(x, y);
PAINTERS.dirt = dirt;
PAINTERS.podzol_top = litter;
PAINTERS.podzol_side = fringed(litter, 0x4e341c, 18);
PAINTERS.stone = stone;
PAINTERS.andesite = grain(
  [
    [0x686868, 4],
    [0x747474, 14],
    [0x7f7f7f, 21],
    [0x8a8a8e, 35],
    [0x9c9c9c, 20],
    [0xa8aa9a, 6],
  ],
  8,
  1,
  0,
  1,
  0.45,
);
PAINTERS.granite = grain(
  [
    [0x5f4034, 7],
    [0x7f5646, 20],
    [0x926251, 16],
    [0x9f6b58, 35],
    [0xa97764, 17],
    [0xb49b91, 5],
  ],
  9,
  1,
  0,
  1,
  0.5,
);
PAINTERS.coal_ore = ore;
PAINTERS.gravel = pebbles;
PAINTERS.sand = sandy([0xd1ba8a, 0xd5c496, 0xdacfa3, 0xe3dbb0, 0xedebcb], 13);
PAINTERS.sandstone = sandstone;
PAINTERS.red_sand = sandy([0xac5712, 0xb2601f, 0xbf6721, 0xcb6e24, 0xd97b30], 10);
PAINTERS.snow = grain(
  [
    [0xf0fdfd, 24],
    [0xf7fefe, 30],
    [0xffffff, 46],
  ],
  17,
  1,
  0,
  1,
  0.5,
);
PAINTERS.grass_block_snow = fringed(PAINTERS.snow, 0xb2d5d5, 19);
PAINTERS.ice = (x, y) => {
  const streak = hash((x + y) >> 1, (x - y + 16) >> 2, 57);
  return streak > 0.93
    ? 0xbcd4ff
    : streak > 0.74
      ? 0xa1c3ff
      : streak > 0.3
        ? 0x92b9fe
        : streak > 0.1
          ? 0x8cb3fe
          : 0x86aefd;
};
PAINTERS.obsidian = grain(
  [
    [0x06030b, 22],
    [0x100c1c, 40],
    [0x1b1529, 26],
    [0x2d2440, 9],
    [0x3b2754, 3],
  ],
  58,
  2,
  0,
  0,
  0.3,
);
PAINTERS.terracotta = clay([0x965d43, 0x985f45, 0x9b6045, 0x9c5d41], 59);
PAINTERS.orange_terracotta = clay([0x9f5224, 0xa25326, 0xa05325, 0xa35527], 60);
PAINTERS.yellow_terracotta = clay([0xb88322, 0xba8624, 0xb98423, 0xbb8624], 74);
PAINTERS.brown_terracotta = clay([0x4d3424, 0x4c3223, 0x4d3324, 0x4e3424], 75);
PAINTERS.red_terracotta = clay([0x8d3b2e, 0x8e3d2f, 0x8e3c2e, 0x903e2f], 76);
PAINTERS.white_terracotta = clay([0xd2b1a1, 0xd1b1a1, 0xcfafa0, 0xd4b6a3], 65);
PAINTERS.light_gray_terracotta = clay([0x876b62, 0x856960, 0x876a61, 0x8a6d63], 77);
PAINTERS.lava_still = lava;
PAINTERS.oak_log = bark([0x382b18, 0x4c3d26, 0x5f4a2b, 0x745a36, 0x917142, 0x987849]);
PAINTERS.oak_log_top = rings([0x4c3d26, 0x5f4a2b], [0x967441, 0x9f844d, 0xb8945f, 0xc29d62]);
PAINTERS.birch_log = birch;
PAINTERS.birch_log_top = rings([0xebebe7, 0xffffff], [0xae9f76, 0xb8a875, 0xc8b77a, 0xd7c185]);
PAINTERS.spruce_log = bark([0x2e1608, 0x2e1c0a, 0x311e0b, 0x3b2713, 0x4d3317, 0x553a1f]);
PAINTERS.spruce_log_top = rings([0x3b2713, 0x553a1f], [0x614b2e, 0x70522e, 0x7a5a34, 0x82613a]);
PAINTERS.dark_oak_log = bark([0x292011, 0x302513, 0x332715, 0x3f311d, 0x4a381e, 0x584428]);
PAINTERS.dark_oak_log_top = rings([0x332715, 0x3f311d], [0x3a2411, 0x3e2912, 0x492f17, 0x4f3218]);
PAINTERS.acacia_log = bark([0x504b40, 0x585349, 0x5b554d, 0x696259, 0x7b7368, 0x8d8477]);
PAINTERS.acacia_log_top = rings([0x575249, 0x696259], [0x99502b, 0xa05630, 0xad5d32, 0xba6337]);
PAINTERS.cherry_log = bark([0x271620, 0x2c1a25, 0x301d29, 0x3b232d, 0x462c39, 0x543543]);
PAINTERS.cherry_log_top = rings([0x271620, 0x3b232d], [0xcd8580, 0xdd9d97, 0xe6b5ad, 0xe7beb4]);
PAINTERS.oak_leaves = foliage([0x686468, 0x777577, 0x989998, 0xb9bcb9], 27);
PAINTERS.birch_leaves = foliage([0x706d70, 0x888787, 0x989998, 0xb7b9b7], 29, 0.74);
PAINTERS.spruce_leaves = foliage([0x424242, 0x555555, 0x8a8a8a, 0x9a9a9a], 31, 0.72);
PAINTERS.dark_oak_leaves = foliage([0x706d70, 0x7d7b7d, 0x989998, 0xb7b9b7], 27, 0.86);
PAINTERS.acacia_leaves = foliage([0x6d6a6a, 0x878787, 0x9e9e9e, 0xb5b5b5], 35, 0.72);
PAINTERS.cherry_leaves = cherryLeaves;
PAINTERS.vine = foliage([0x545454, 0x6b6b6b, 0x838383, 0xaaaaaa], 39, 0.6);
PAINTERS.cactus_side = cactusSide;
PAINTERS.cactus_top = cactusTop;
PAINTERS.short_grass = (x, y) => shortGrass(x, 15 - y);
PAINTERS.tall_grass_bottom = (x, y) => tallGrass(x, 15 - y);
PAINTERS.tall_grass_top = (x, y) => tallGrass(x, 31 - y);
PAINTERS.fern = drawn(
  [0x585958, 0x6b6e6b, 0x7c7e7c, 0x8c8a8c, 0x949194, 0xa5a2a5],
  [
    '.......2........',
    '......141.......',
    '.......4........',
    '.....23532......',
    '....1.252.1.....',
    '...2314541320...',
    '..1..23532..1...',
    '.213.14541.312..',
    '0.2.2325232.2.0.',
    '.1324145414231..',
    '..0.23353320.0..',
    '....1254521.....',
    '.....13531......',
    '......252.......',
    '.......3........',
    '......141.......',
  ],
);
PAINTERS.poppy = drawn(
  [0x204626, 0x265a25, 0x2b702a, 0x4a8f28, 0x742303, 0x9b221a, 0xbf2529, 0xed302c],
  [
    '.......77.......',
    '......7657......',
    '.....765567.....',
    '......6776......',
    '.......52.......',
    '........3.......',
    '........3.......',
    '........2.......',
    '.....12.3.21....',
    '......1.21......',
    '.......121......',
  ],
);
PAINTERS.dandelion = drawn(
  [...STEM, 0xbd6a22, 0xf19d25, 0xfed639, 0xffec4f],
  [
    '.......66.......',
    '......6556......',
    '.....654456.....',
    '......6436......',
    '.......10.......',
    '.......2........',
    '.....0121.......',
    '......02110.....',
    '.......10.......',
  ],
);
PAINTERS.cornflower = drawn(
  [0x2e6924, 0x3f7935, 0x578c4d, 0x432e99, 0x2a4cc7, 0x466aeb, 0x6281e8, 0x728ff1, 0x8e6cc3],
  [
    '....7.7.56......',
    '....577565......',
    '.....64384......',
    '.......12.......',
    '.......22.......',
    '........1.......',
    '....2...2.......',
    '.....2..2.......',
    '.....12.2.......',
    '......122.......',
    '.......12.......',
    '........0.......',
  ],
);
PAINTERS.oxeye_daisy = drawn(
  [...STEM, 0x9bbdbd, 0xd6e8e8, 0xf7f7f7, 0xf19d25, 0xfed639, 0xffec4f],
  [
    '......545.......',
    '.....45554......',
    '....4578754.....',
    '....5576755.....',
    '....3478654.....',
    '.....45544......',
    '......453.......',
    '.......1........',
    '.......2........',
    '.....012........',
    '......02.0......',
    '.......21.......',
    '.......1........',
    '.......0........',
  ],
);
PAINTERS.azure_bluet = drawn(
  [...STEM, 0xffec4f, 0xd6e8e8, 0xf7f7f7],
  [
    '......4.........',
    '.....535..4.....',
    '......4..535....',
    '...4.21...4.....',
    '..535.02.1......',
    '...4.1.2104.....',
    '....021.1535....',
    '.....1202.4.....',
    '......211.......',
    '.......1........',
    '.......0........',
  ],
);
PAINTERS.allium = drawn(
  [...STEM, 0x7b4ea0, 0xa65ee1, 0xb878ed, 0xd2a6f6, 0xe8cffe],
  [
    '......654.......',
    '.....67653......',
    '....6767543.....',
    '....5656543.....',
    '.....45454......',
    '......314.......',
    '.......1........',
    '.......2........',
    '.......2........',
    '.......1........',
    '.......2........',
    '.......1........',
    '.......1........',
    '.......0........',
  ],
);
PAINTERS.blue_orchid = drawn(
  [...STEM, 0x1c92d6, 0x27a9f4, 0x2abffd, 0x2fcefd],
  [
    '.......45.......',
    '......3464......',
    '.....43.53......',
    '......4.1.34....',
    '.......1.453....',
    '....34.2..4.....',
    '...3541.2.......',
    '....4..12.......',
    '.......2........',
    '.....1.1.2......',
    '......121.......',
    '.......0........',
  ],
);
PAINTERS.dead_bush = drawn(
  [0x513d24, 0x67502c, 0x946428, 0xb17a36],
  [
    '..........0.....',
    '....2....10.....',
    '....12..1.1.....',
    '.....2..20......',
    '.....12.1..0....',
    '......231.01....',
    '..00..121.1..0..',
    '...011.2312.10..',
    '.....01230210...',
    '......01220.....',
    '.......022......',
    '....01..21......',
    '.....0111.......',
    '......1011.1....',
    '.......0101.....',
    '.......000......',
  ],
);
PAINTERS.sugar_cane = cane;
PAINTERS.red_mushroom = drawn(
  [0xa9121a, 0xc41d26, 0xe21212, 0xfe2a2a, 0xede8ca, 0xcfc47f],
  [
    '......2332......',
    '.....232432.....',
    '....22342221....',
    '....41221120....',
    '.......54.......',
    '.......45.......',
  ],
);
PAINTERS.brown_mushroom = drawn(
  [0x4c3d33, 0x725643, 0x916d55, 0xcc9978, 0xb5947d, 0xe0d3c0],
  ['......3333......', '....23333322....', '....12222110....', '.......54.......', '.......54.......'],
);
PAINTERS.lily_pad = lily;
PAINTERS.pink_petals = petals;

PAINTERS.crying_obsidian = (x, y) =>
  hash(x >> 1, (y + x) >> 1, 96) > 0.86 ? (hash(x, y, 97) > 0.5 ? 0xa855f7 : 0x7b2fd0) : PAINTERS.obsidian!(x, y);
PAINTERS.netherrack = grain(
  [
    [0x501b1b, 14],
    [0x6a2a2a, 36],
    [0x7b3535, 32],
    [0x8e4545, 18],
  ],
  98,
  1,
  0,
  1,
  0.5,
);
PAINTERS.magma = (x, y) => {
  const crack = hash((x + (y >> 1)) >> 1, (y + (x >> 2)) >> 1, 99);
  return crack > 0.78
    ? hash(x, y, 100) > 0.5
      ? 0xf8a030
      : 0xe06010
    : crack > 0.66
      ? 0x8a2a10
      : hash(x, y, 101) > 0.5
        ? 0x3a1a14
        : 0x4a221a;
};

/** Painted stand-ins for a game's block textures, for the mock API and for tests: sixteen rows of RGBA per name. */
export function paintedTextures(): Map<string, Uint8Array> {
  const pack = new Map<string, Uint8Array>();
  for (const [name, paint] of Object.entries(PAINTERS)) {
    const rgba = new Uint8Array(1024);
    for (let i = 0; i < 256; i++) {
      const color = paint(i & 15, i >> 4);
      if (color < 0) continue;
      rgba.set([color >> 16, (color >> 8) & 255, color & 255, 255], i * 4);
    }
    pack.set(name, rgba);
  }
  return pack;
}
