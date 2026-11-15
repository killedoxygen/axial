import assert from 'node:assert/strict';
import test from 'node:test';

import * as packing from '../src/vista/drawing/packing';
import { createRenderer } from '../src/vista/drawing/render';
import { Sheet } from '../src/vista/drawing/strips';
import { CHOICES, buildWorld, chosen, type Choice } from '../src/vista/drawing/world';

const WIDTH = 208;
const HEIGHT = 90;

/** A picture drawn as the workers draw it: each renderer takes strips, then finishes its own for the one sheet. */
function picture(seed: number, choice?: Choice, strips: [number, number][][] = [[[0, WIDTH]]]): Uint8ClampedArray {
  const sheet = new Sheet(WIDTH, HEIGHT);
  const renderers = strips.map((own) => {
    const renderer = createRenderer(buildWorld(seed, choice), sheet.width, sheet.height);
    for (const [from, to] of own) sheet.lay(renderer.draw(from, to));
    return renderer;
  });
  for (const renderer of renderers) {
    for (const strip of renderer.finish(sheet.soft)) sheet.set(strip);
  }
  return sheet.pixels;
}

/** Whether a pixel is as bright and pale as a lit window: the sky's glow and lava are redder. */
const lit = (pixels: Uint8ClampedArray, at: number): boolean => pixels[at]! > 240 && pixels[at + 1]! > 220;

test('a seed always draws the same world, however its columns are shared out', () => {
  const whole = picture(4242);
  const shared = picture(4242, undefined, [
    [
      [120, WIDTH],
      [0, 64],
    ],
    [[64, 120]],
  ]);
  assert.deepEqual(shared, whole);
  assert.notDeepEqual(picture(4243), whole);
});

test('every seed fills the whole picture, neither black nor burnt out', () => {
  for (let seed = 1; seed <= 60; seed++) {
    const pixels = picture(seed * 2654435761);
    let light = 0;
    for (let i = 0; i < pixels.length; i += 4) {
      assert.equal(pixels[i + 3], 255, `seed ${seed} leaves a hole`);
      light += pixels[i]! + pixels[i + 1]! + pixels[i + 2]!;
    }
    const mean = light / (pixels.length / 4) / 3;
    assert.ok(mean > 6 && mean < 190, `seed ${seed} mean ${mean.toFixed(0)}`);
  }
});

test('every picture shows its building lit, right of the middle, where a page expects it', () => {
  // Every land in each of its biomes with one building, and every building in one place.
  const some = CHOICES.filter(
    ({ land, biome, building }) => building === 'cabin' || (land === 'lake' && biome === 'taiga'),
  );
  for (const choice of some) {
    const name = `${choice.land} ${choice.biome} ${choice.building}`;
    const pixels = picture(7, choice);
    const { foot, top } = buildWorld(7, choice).camera;
    assert.ok(top > 0.02 && foot <= 0.82 && foot - top > 0.08, `${name}: stands from ${top} to ${foot}`);
    let inside = 0;
    let above = 0;
    for (let y = 0; y < HEIGHT; y++) {
      for (let x = WIDTH >> 1; x < WIDTH; x++) {
        if (!lit(pixels, (y * WIDTH + x) * 4)) continue;
        if (y < (top - 0.03) * HEIGHT) above++;
        else if (y <= foot * HEIGHT) inside++;
      }
    }
    assert.ok(inside >= 8, `${name}: ${inside} lit pixels between its top and its foot`);
    assert.equal(above, 0, `${name}: lit above its top`);
  }
});

test('seeds spread over the pictures there are', () => {
  const had = new Set<unknown>();
  for (let seed = 1; seed <= 12000; seed++) had.add(chosen(seed));
  assert.ok(CHOICES.length >= 500);
  assert.ok(had.size > CHOICES.length * 0.95, `${had.size} of ${CHOICES.length}`);
});

test('a packed picture unpacks to itself, at a fraction of the size', () => {
  const pixels = picture(4242);
  const file = packing.pack(pixels, WIDTH, HEIGHT);
  assert.ok(file.length < pixels.length / 2);
  assert.deepEqual(packing.unpack(file), { pixels, width: WIDTH, height: HEIGHT });
});
