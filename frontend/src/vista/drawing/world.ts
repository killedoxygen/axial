/**
 * An instance's world, from its seed: water at dusk, a lit building on its far shore, the country rising round it.
 * A world is one lie of the land, one of the game's biomes that is found in such a lie, and one building, each
 * from a short list set down by hand. What the ground is, what grows on it and what the building is made of are
 * the biome's own, as the game has them. The building always stands right of the middle, where a page shows the
 * picture whole.
 */
import { BIOMES, type Biome } from './biomes';
import * as B from './blocks';
import { BUILDINGS, tallness, type Building } from './buildings';
import { GRID, ease, fbm, hash, noise } from './land';
import { LANDS, type Land, type LandName } from './lands';
import { palette } from './paints';
import { ASPECT } from './strips';
import { WOOD, grow, type Plot } from './trees';

/** An evening's colours: the low glow ahead, the sky away from it, the light from behind, and the air. */
export interface Dusk {
  glow: readonly [number, number, number];
  cool: readonly [number, number, number];
  fill: readonly [number, number, number];
  haze: readonly [number, number, number];
}

export interface World {
  seed: number;
  ground: Uint8Array;
  groundMat: Uint8Array;
  /** Per column: `runCount` runs of (bottom, top, material), bottom to top. */
  runs: Uint8Array;
  runCount: Uint8Array;
  /** The water's level, or 0 where it is frozen over. */
  waterLevel: number;
  /** Per column, the sky's light at its surface, on the game's scale of 0 to 15. */
  skyLight: Uint8Array;
  /**
   * What gives light to the land round it, as points of light, five values each: where, how strong, and a column
   * that is not in its way (-1 for none).
   */
  lamps: Float32Array;
  /** One colour for each paint, as `palette` gives them. */
  colors: Float32Array;
  /** Unit vector toward the last light, and the evening's colours. */
  light: readonly [number, number, number];
  dusk: Dusk;
  /**
   * The lens is the angle a whole picture spans. The horizon, the building's foot and its top lie at shares of a
   * whole picture's height.
   */
  camera: { x: number; y: number; z: number; yaw: number; fov: number; horizon: number; foot: number; top: number };
}

/** The way runs of blocks are added to a world's columns: `put(x, z, y0, y1, material)`. */
function putter(ground: Uint8Array, runs: Uint8Array, runCount: Uint8Array): Plot['put'] {
  const n = GRID;
  return (x, z, y0, y1, mat) => {
    if (x < 0 || z < 0 || x >= n || z >= n || y1 <= y0 || y0 < ground[z * n + x]!) return;
    const i = z * n + x;
    const count = runCount[i]!;
    const base = i * B.RUNS * 3;
    // Runs stay sorted bottom to top. One that meets a run of its own kind joins it; one that would overlap
    // anything else is dropped.
    let at = count;
    while (at > 0 && runs[base + (at - 1) * 3]! >= y1) at--;
    if (at > 0 && runs[base + (at - 1) * 3 + 1]! >= y0) {
      const o = base + (at - 1) * 3;
      if (runs[o + 2] === mat && runs[o]! <= y0) runs[o + 1] = Math.max(runs[o + 1]!, y1);
      if (runs[o + 2] === mat || runs[o + 1]! > y0) return;
    }
    if (count >= B.RUNS) return;
    for (let k = count; k > at; k--) runs.copyWithin(base + k * 3, base + k * 3 - 3, base + k * 3);
    runs.set([y0, Math.min(255, y1), mat], base + at * 3);
    runCount[i] = count + 1;
  };
}

/**
 * The sky's light on each column, as the game spreads it: thinned by a level for each layer of foliage it passes,
 * shut out under anything solid, and one level lost for every block it travels across.
 */
function skyLit(runs: Uint8Array, runCount: Uint8Array): Uint8Array {
  const n = GRID;
  const skyLight = new Uint8Array(n * n);
  for (let i = 0; i < n * n; i++) {
    let shade = 0;
    for (let k = 0; k < runCount[i]!; k++) {
      const o = (i * B.RUNS + k) * 3;
      const mat = runs[o + 2]!;
      if (mat < B.FOLIAGE) shade = 15;
      else if (mat < B.LAMP) shade += runs[o + 1]! - runs[o]!;
    }
    skyLight[i] = Math.max(0, 15 - shade);
  }
  const spread = (from: number, to: number): void => {
    if (from >= 0 && from < n * n && skyLight[from]! - 1 > skyLight[to]!) skyLight[to] = skyLight[from]! - 1;
  };
  for (let pass = 0; pass < 4; pass++) {
    const forward = (pass & 1) === 0;
    for (let k = 0; k < n * n; k++) {
      const i = forward ? k : n * n - 1 - k;
      const x = i % n;
      spread(forward ? (x > 0 ? i - 1 : -1) : x < n - 1 ? i + 1 : -1, i);
      spread(forward ? i - n : i + n, i);
    }
  }
  return skyLight;
}

const SEA = 14;
/** A land's own ground lies about a middle well inside the grid. */
const X0 = -110;
const Y0 = -52;

/** What a picture is of: which lie of the land, which biome, which building. */
export interface Choice {
  land: LandName;
  biome: string;
  building: keyof typeof BUILDINGS;
}

/** The lens: the angle a picture spans, and the rows of a whole picture, as a share of its height, to a unit of slope. */
const FOV = 2 * Math.atan(18 / 35);
const ROWS = ASPECT / (2 * Math.tan(FOV / 2));
/**
 * Where a building's foot stands in every picture, as a share of its height, so that a page may show any picture
 * the same way; and the most of a picture's height a building may take: a taller one is not put on that land.
 */
const FOOT = 0.66;
const TALLEST = 0.5;

/** How high the land stands above the water at a place, before anything is built on it. */
function rise(land: Land, biome: Biome, xs: number, ys: number): number {
  const { seed, water } = land;
  // A biome's land lies as rolling and stands as high as the game has it, against the spruce hills' own.
  const rolling = Math.min(1.1, Math.max(0.45, biome.relief / 7));
  const lofty = Math.min(1.4, Math.max(0.3, biome.rise / 26));
  const d = Math.hypot((xs - water.x) / water.wide, ys - water.y);
  const n1 = fbm(xs * 0.022, ys * 0.022, seed);
  const n2 = fbm(xs * 0.07 + 40, ys * 0.07 + 40, seed + 5, 3);
  const n3 = fbm(xs * 0.011 + 90, ys * 0.011 + 90, seed + 9, 3);
  const shore = Math.min(2.2, Math.max(-4, (d - water.r) * 0.24));
  const front = ease(8, -18, ys);
  const hills = Math.max(0, d - water.r - 6) * 0.13 * land.hills * rolling * (1 + 0.9 * n1) * (1 - 0.85 * front);
  const far = ease(60, 150, ys) * (10 + 12 * n3 + 4 * n1) * land.far * lofty;
  const aside = land.open ? xs - water.x : Math.abs(xs - water.x);
  // An open land's rise is a cape: it runs out to sea again further off.
  const cape = land.open ? ease(125, 85, ys) : 1;
  const sides = ease(34, 95, aside) * (9 + 9 * n1) * land.sides * Math.sqrt(lofty) * (1 - 0.6 * front) * cape;
  const near = ease(-22, -46, ys) * (1.5 + 1.5 * n2);
  const h = Math.round(shore + hills + far + sides + near + 1.3 * n2);
  // Flat-topped steps with sheer sides.
  return biome.mesa && h > 4 ? 4 + Math.floor((h - 4) / 7) * 8 : h;
}

/** How much of a picture's height a building takes on a land, and how far off it stands. */
function sized(land: Land, building: Building): { span: number; off: number; aheadX: number; aheadY: number } {
  const { camera, look, site } = land;
  const ahead = Math.hypot(look.x - camera.x, look.y - camera.y);
  const aheadX = (look.x - camera.x) / ahead;
  const aheadY = (look.y - camera.y) / ahead;
  // The building stands about the middle of its site, whatever its width.
  const off = (site.x + 5 - camera.x) * aheadX + (site.y - camera.y) * aheadY;
  return { span: (tallness(building) / off) * ROWS, off, aheadX, aheadY };
}

/**
 * How a picture is taken: from the near shore, level, a little above the ground, looking the land's own way. The
 * horizon is set so that the building's foot stands at one height in every picture.
 */
function framing(
  land: Land,
  biome: Biome,
  building: Building,
): { eye: number; aheadX: number; aheadY: number; horizon: number; top: number } {
  const { camera, site } = land;
  const { span, off, aheadX, aheadY } = sized(land, building);
  const eye = SEA + rise(land, biome, camera.x + 0.5, camera.y + 0.5) + 5;
  return { eye, aheadX, aheadY, horizon: FOOT - ((eye - SEA - site.rise) / off) * ROWS, top: FOOT - span };
}

/**
 * Every picture there is: each land in each biome found in such a lie, with each building that is not too tall
 * for it.
 */
export const CHOICES: readonly Choice[] = (Object.keys(LANDS) as Choice['land'][]).flatMap((land) =>
  BIOMES.filter(({ lies }) => lies.includes(LANDS[land].lie)).flatMap(({ name }) =>
    (Object.keys(BUILDINGS) as Choice['building'][])
      .filter((building) => sized(LANDS[land], BUILDINGS[building]).span <= TALLEST)
      .map((building) => ({ land, biome: name, building })),
  ),
);

/** The picture a seed has. */
export const chosen = (seed: number): Choice =>
  CHOICES[Math.floor(hash(seed, 5, 71) * CHOICES.length) % CHOICES.length]!;

const named = (choice: Choice): [Land, Biome, Building] => [
  LANDS[choice.land],
  BIOMES.find(({ name }) => name === choice.biome)!,
  BUILDINGS[choice.building],
];

/** The world a seed has; or, for looking at one, the world of a `choice`. */
export function buildWorld(seed: number, choice = chosen(seed)): World {
  const [land, biome, building] = named(choice);
  const { seed: SEED, camera: CAMERA, look: LOOK, water: LAKE } = land;
  const YARD = land.site.rise;
  // The building stands about the middle of its site, whatever its width.
  const x0 = land.site.x + 5 - (building.wide >> 1);
  const y0 = land.site.y;
  // What gives light by the shore lies beside the building, clear of it, on the side of the water's middle.
  const n = GRID;
  const ground = new Uint8Array(n * n);
  const groundMat = new Uint8Array(n * n);
  const runs = new Uint8Array(n * n * B.RUNS * 3);
  const runCount = new Uint8Array(n * n);
  const door = x0 + building.door;
  const add = putter(ground, runs, runCount);
  /** A block of the scene, by its own coordinates: across, away from the camera, and up from the water. */
  const put = (x: number, y: number, z: number, mat: number, tall = 1): void =>
    add(x - X0, y - Y0, SEA + z, SEA + z + tall, mat);
  const heights = new Int16Array(n * n);
  const top = (x: number, y: number): number => heights[(y - Y0) * n + x - X0]!;
  /** How far outside the building's footprint a place lies. */
  const yard = (x: number, y: number): number =>
    Math.hypot(
      Math.max(0, Math.abs(x + 0.5 - x0 - building.wide / 2) - building.wide / 2),
      Math.max(0, Math.abs(y + 0.5 - y0 - building.deep / 2) - building.deep / 2),
    );

  const roughs = new Float32Array(n * n);
  for (let j = 0; j < n; j++) {
    for (let i = 0; i < n; i++) {
      const xs = i + X0 + 0.5;
      const ys = j + Y0 + 0.5;
      let h = rise(land, biome, xs, ys);
      // A flat yard for the building, eased into the land round it.
      const out = yard(i + X0, j + Y0);
      if (out < 4.5) h = YARD;
      else if (out < 9.5) h = Math.round(YARD + (h - YARD) * ease(4.5, 9.5, out));
      heights[j * n + i] = h;
      // Frozen water is a floor of ice.
      ground[j * n + i] = SEA + (biome.frozen && h < 0 ? 0 : h);
      roughs[j * n + i] = (fbm(xs * 0.07 + 40, ys * 0.07 + 40, SEED + 5, 3) + 1) / 2;
    }
  }
  // The ground, as the game lays it: the bed under water, the shore by it, bare rock where the land is steep or
  // stands high, then snow; else the biome's own surface, with its patches.
  const sandy = biome.surface === B.SAND || biome.surface === B.RED_SAND;
  const rock = sandy ? B.SANDSTONE : B.STONE;
  for (let j = 0; j < n; j++) {
    for (let i = 0; i < n; i++) {
      const at = j * n + i;
      const h = heights[at]!;
      const rough = roughs[at]!;
      const inner = i > 0 && j > 0 && i < n - 1 && j < n - 1;
      const steep = inner ? h - Math.min(heights[at - 1]!, heights[at + 1]!, heights[at - n]!, heights[at + n]!) : 0;
      const patchy = noise((i + X0) / 9, (j + Y0) / 9, SEED + 15);
      if (h < 0) groundMat[at] = biome.frozen ? B.ICE : patchy > 0.3 && biome.bed !== B.RED_SAND ? B.GRAVEL : biome.bed;
      else if (biome.mesa && h > 4) groundMat[at] = B.TERRACOTTA;
      else if (h > (biome.frozen ? 14 : 46) + rough * 8) groundMat[at] = B.SNOW;
      else if (steep > 2 || (biome.alpine && h > 36 + rough * 8)) groundMat[at] = rock;
      else if (h <= 1) groundMat[at] = biome.shore;
      else if (biome.patches && patchy > 0.25) groundMat[at] = biome.patches;
      else groundMat[at] = biome.surface;
    }
  }

  // Light the land gives of itself. In dry country lava lies open, a pool on the shore beside the building; in
  // high country it springs from the rock and runs down to the water. Trees keep off both.
  const lamps: number[] = [];
  const molten = new Uint8Array(n * n);
  const flow: [number, number][] = [];
  const lava = (i: number, j: number): void => {
    groundMat[j * n + i] = B.LAVA;
    flow.push([i + X0, j + Y0]);
    for (let dj = -3; dj <= 3; dj++) for (let di = -3; di <= 3; di++) molten[(j + dj) * n + i + di] = 1;
  };
  const beside = land.site.x > LAKE.x ? x0 - 8 : x0 + building.wide + 7;
  if (sandy) {
    const px = beside;
    for (let py = y0 + 10; py > Y0 + 4; py--) {
      if (!(top(px, py) > 1 && top(px, py - 1) <= 1)) continue;
      const floor = top(px, py + 3);
      for (let dy = -3; dy <= 3; dy++) {
        for (let dx = -5; dx <= 5; dx++) {
          const at = (py + 3 + dy - Y0) * n + px + dx - X0;
          if ((dx / 5.2) ** 2 + (dy / 3.2) ** 2 > 1 + (hash(px + dx, py + dy, seed + 77) - 0.5) * 0.5) continue;
          heights[at] = floor - 1;
          ground[at] = SEA + floor - 1;
          lava(px + dx - X0, py + 3 + dy - Y0);
        }
      }
      lamps.push(px - X0 + 0.5, SEA + floor + 1.2, py + 3.5 - Y0, 700 / (4 * Math.PI * Math.PI), -1);
      break;
    }
  } else if (land.lie === 'range' || land.lie === 'fjord' || land.lie === 'cliffs') {
    // The spring: the first ground high enough, going up the slope beside the building.
    let i = beside - X0;
    let j = y0 + 2 - Y0;
    while (j < n - 8 && heights[j * n + i]! < 13) j++;
    for (let step = 0; step < 90 && j < n - 8; step++) {
      const h = heights[j * n + i]!;
      // Lava never lies in water: where it would meet it, it has set to stone.
      if (h <= 1) {
        if (h > 0) groundMat[j * n + i] = B.STONE;
        break;
      }
      // Bare rock either side of it, and the lava in its bed.
      for (const [di, dj] of [
        [1, 0],
        [-1, 0],
        [0, 1],
        [0, -1],
      ] as const)
        if (groundMat[(j + dj) * n + i + di] !== B.LAVA && heights[(j + dj) * n + i + di]! > 0)
          groundMat[(j + dj) * n + i + di] = rock;
      lava(i, j);
      if (hash(i, j, seed + 85) < 0.6 && heights[j * n + i + 1]! > 1) lava(i + 1, j);
      if (step % 6 === 0) lamps.push(i + 0.5, SEA + h + 1.2, j - 0.6, 260 / (4 * Math.PI * Math.PI), -1);
      // It runs the steepest way down, and toward the water where the ground is level.
      let next = j - 1;
      let across = i;
      let lowest = heights[(j - 1) * n + i]!;
      for (const di of [-1, 1]) {
        if (heights[j * n + i + di]! < lowest) {
          lowest = heights[j * n + i + di]!;
          next = j;
          across = i + di;
        }
      }
      if (lowest > h) break;
      i = across;
      j = next;
    }
  }

  // Under the water, seldom: a block of magma on the deep of the bed, or two or three together. A warm sea has a
  // few sea pickles in its shallows.
  if (!biome.frozen && hash(seed, 3, 91) < 0.3) {
    const spots = 1 + Math.floor(hash(seed, 5, 93) * 2);
    for (let spot = 0; spot < spots; spot++) {
      // Somewhere in the water between where it is seen from and the building.
      const along = 0.4 + 0.3 * spot + hash(seed, spot, 95) * 0.15;
      const i = Math.round(CAMERA.x + (door - CAMERA.x) * along + (hash(seed, spot, 97) - 0.5) * 16) - X0;
      const j = Math.round(CAMERA.y + (y0 - CAMERA.y) * along) - Y0;
      const blocks = 1 + Math.floor(hash(seed, spot, 99) * 3);
      for (let block = 0; block < blocks; block++) {
        const at = (j + (block === 2 ? 1 : 0)) * n + i + (block === 1 ? 1 : 0);
        if (heights[at]! <= -3) groundMat[at] = B.MAGMA;
      }
    }
  }
  if (land.open && (biome.name === 'savanna' || biome.name === 'desert')) {
    for (let j = 8; j < n - 8; j++)
      for (let i = 8; i < n - 8; i++) {
        const h = heights[j * n + i]!;
        if (h < 0 && h >= -3 && hash(i, j, seed + 75) < 0.012) groundMat[j * n + i] = B.SEA_PICKLES;
      }
  }

  // Trees, in the biome's own kinds and shares, as thick as it grows them. None in the yard, over the camera,
  // across its view of the water, or before the building; and only in ground their kind takes root in.
  const plot: Plot = { seed, ground, put: add };
  const main = biome.trees[0]![0];
  let shares = 0;
  for (const [, share] of biome.trees) shares += share;
  const fertile = (mat: number): boolean => mat === B.GRASS || mat === B.PODZOL || mat === B.SNOWY_GRASS;
  const slope = (LOOK.x - CAMERA.x) / (LOOK.y - CAMERA.y);
  const reach = Math.hypot(LOOK.x - CAMERA.x, LOOK.y - CAMERA.y);
  const toX = (LOOK.x - CAMERA.x) / reach;
  const toY = (LOOK.y - CAMERA.y) / reach;
  /** How far off a place is, and how far to the right of straight ahead it is seen, as a slope. */
  const seen = (x: number, y: number): [number, number] => {
    const off = (x - CAMERA.x) * toX + (y - CAMERA.y) * toY;
    return [off, ((x - CAMERA.x) * toY - (y - CAMERA.y) * toX) / off];
  };
  const corners = [
    seen(x0, y0),
    seen(x0 + building.wide, y0),
    seen(x0, y0 + building.deep),
    seen(x0 + building.wide, y0 + building.deep),
  ];
  const nearest = Math.min(...corners.map(([off]) => off));
  const leftmost = Math.min(...corners.map(([, across]) => across));
  const rightmost = Math.max(...corners.map(([, across]) => across));
  /** Whether a tree at a place would stand before the building. */
  const hides = (x: number, y: number): boolean => {
    const [off, across] = seen(x + 0.5, y + 0.5);
    return off > 0 && off < nearest && across > leftmost - 5 / off && across < rightmost + 5 / off;
  };
  // Nor does one stand before the lava, on the side it is seen from.
  const lit = flow.map(([x, y]) => seen(x + 0.5, y + 0.5));
  const veils = (x: number, y: number): boolean => {
    const [off, across] = seen(x + 0.5, y + 0.5);
    return off > 0 && lit.some(([far, at]) => off < far && Math.abs(across - at) < 4 / off);
  };
  for (let j = 4; j < n - 9; j++) {
    for (let i = 8; i < n - 9; i++) {
      const x = i + X0;
      const y = j + Y0;
      const h = top(x, y);
      const mat = groundMat[j * n + i]!;
      // A swamp's trees stand in its shallows too; a cactus wants sand or clay.
      if (
        h < 0
          ? !biome.flooded || h < -2
          : main === 'cactus'
            ? mat !== biome.surface && mat !== B.TERRACOTTA
            : !fertile(mat)
      )
        continue;
      if (yard(x, y) < 7.5 || hides(x, y) || molten[j * n + i]) continue;
      if (
        Math.hypot(x - CAMERA.x, y - CAMERA.y) < 24 ||
        (Math.abs(x - CAMERA.x - (y - CAMERA.y) * slope) < 10 && y < 8)
      )
        continue;
      if (Math.abs(top(x + 1, y) - h) + Math.abs(top(x, y + 1) - h) > 2) continue;
      const forest = fbm((x + 0.5) * 0.05 + 7, (y + 0.5) * 0.05 + 7, SEED + 3, 3);
      const density = ((0.01 + 0.05 * Math.max(0, forest + 0.25) + 0.0006 * Math.min(h, 30)) * biome.density) / 0.55;
      if (hash(x, y, seed + 41) >= density || runCount[j * n + i]! > 0) continue;
      if (veils(x, y)) continue;
      let pick = hash(x, y, seed + 43) * shares;
      let kind = main;
      for (const [each, share] of biome.trees) {
        kind = each;
        if ((pick -= share) < 0) break;
      }
      grow(plot, kind, i, j, hash(x, y, seed + 47), biome.frozen || mat === B.SNOW);
    }
  }
  // A tree or two of the biome's chief kind near enough to frame the view, where it has trees.
  if (WOOD[main].length > 1)
    for (const [x, y, tall] of land.frame)
      if (!hides(x, y) && fertile(groundMat[(y - Y0) * n + x - X0]!))
        grow(plot, main, x - X0, y - Y0, tall / 12, !!biome.frozen);

  // Posts and corner posts are of the biome's chief tree, or of what it builds with where it has none.
  const timber = WOOD[main].length > 1 ? WOOD[main][0]! : biome.walls;
  // The building, on cobbles; then a path from its door to the water and a dock.
  const lamp = (x: number, y: number, z: number): void => {
    put(x, y, z, B.LAMP);
    // Its light hangs on the side the picture is seen from, so it falls on what faces that way.
    lamps.push(x - X0 + 0.5, SEA + z + 0.5, y - Y0 - 0.4, 420 / (4 * Math.PI * Math.PI), (y - Y0) * n + x - X0);
  };
  for (const [px, py, wide, deep] of building.plots ?? [[0, 0, building.wide, building.deep]])
    for (let x = x0 + px - 1; x <= x0 + px + wide; x++)
      for (let y = y0 + py - 1; y <= y0 + py + deep; y++) groundMat[(y - Y0) * n + x - X0] = B.GRAVEL;
  building.raise({
    walls: biome.walls,
    roof: biome.roof,
    timber,
    put: (x, y, z, mat, tall) => put(x0 + x, y0 + y, YARD + z, mat, tall),
    lamp: (x, y, z) => lamp(x0 + x, y0 + y, YARD + z),
  });
  let y = y0 - 2;
  while (top(door, y) > 0 && y > Y0 + 2) {
    groundMat[(y - Y0) * n + door - X0] = B.GRAVEL;
    groundMat[(y - Y0) * n + door + 1 - X0] = B.GRAVEL;
    y--;
  }
  for (let k = 0; k < 9; k++) {
    put(door, y - k, 0, biome.walls);
    put(door + 1, y - k, 0, biome.walls);
    if (k % 4 === 0 && k < 8) {
      put(door - 1, y - k, -1, timber);
      put(door + 2, y - k, -1, timber);
    }
  }
  for (const dx of [-1, 2]) put(door + dx, y - 8, -1, timber, 2);
  // Light spilling from the windows onto the yard.
  lamps.push(door + 1 - X0, SEA + YARD + 2, y0 - 2.2 - Y0, 900 / (4 * Math.PI * Math.PI), -1);

  const { eye, aheadX, aheadY, horizon, top: head } = framing(land, biome, building);
  // The last light is low ahead.
  const low = (14 * Math.PI) / 180;
  return {
    seed: SEED,
    ground,
    groundMat,
    runs,
    runCount,
    waterLevel: biome.frozen ? 0 : SEA,
    skyLight: skyLit(runs, runCount),
    lamps: Float32Array.from(lamps),
    colors: palette(biome),
    light: [aheadX * Math.cos(low), Math.sin(low), aheadY * Math.cos(low)],
    dusk: biome.dusk,
    camera: {
      x: CAMERA.x - X0,
      y: eye,
      z: CAMERA.y - Y0,
      yaw: Math.atan2(aheadX, aheadY),
      fov: FOV,
      horizon,
      foot: FOOT,
      top: head,
    },
  };
}
