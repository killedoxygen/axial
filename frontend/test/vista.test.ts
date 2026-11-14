import assert from 'node:assert/strict';
import test from 'node:test';

import { paintedTextures } from '../src/mock/block-textures';
import * as packing from '../src/vista/drawing/packing';
import { createRenderer } from '../src/vista/drawing/render';
import { Sheet } from '../src/vista/drawing/strips';
import { buildWorld } from '../src/vista/drawing/world';

const pack = paintedTextures();

/** A picture drawn as the workers draw it: each renderer takes strips, then finishes its own for the one sheet. */
function picture(seed: number, dark: boolean, strips: [number, number][][] = [[[0, 208]]]): Uint8ClampedArray {
  const sheet = new Sheet(208, 90);
  const renderers = strips.map((own) => {
    const renderer = createRenderer(buildWorld(seed, dark, pack)!, sheet.width, sheet.height);
    for (const [from, to] of own) sheet.lay(renderer.draw(from, to));
    return renderer;
  });
  for (const renderer of renderers) {
    for (const strip of renderer.finish(sheet.soft, sheet.open)) sheet.set(strip);
  }
  return sheet.pixels;
}

test('a seed always draws the same world, however its columns are shared out', () => {
  const whole = picture(4242, true);
  const shared = picture(4242, true, [
    [
      [120, 208],
      [0, 64],
    ],
    [[64, 120]],
  ]);
  assert.deepEqual(shared, whole);
  assert.notDeepEqual(picture(4243, true), whole);
});

test('every seed fills the whole picture in both colour modes', () => {
  for (let seed = 1; seed <= 60; seed++) {
    for (const dark of [true, false]) {
      const pixels = picture(seed * 2654435761, dark);
      let lit = 0;
      for (let i = 0; i < pixels.length; i += 4) {
        assert.equal(pixels[i + 3], 255, `seed ${seed} leaves a hole`);
        lit += pixels[i]! + pixels[i + 1]! + pixels[i + 2]!;
      }
      const mean = lit / (pixels.length / 4) / 3;
      assert.ok(
        dark ? mean > 6 && mean < 190 : mean > 60,
        `seed ${seed} ${dark ? 'dark' : 'light'} mean ${mean.toFixed(0)}`,
      );
    }
  }
});

test('a version shows only what it has, and nothing without the ground to stand on', () => {
  const old = new Map([...pack].filter(([name]) => !name.includes('cherry') && name !== 'pink_petals'));
  for (let seed = 1; seed <= 40; seed++) assert.ok(buildWorld(seed, true, old), `seed ${seed} builds no world`);
  assert.equal(buildWorld(1, true, new Map()), null);
});

test('a packed picture unpacks to itself, at a fraction of the size', () => {
  const pixels = picture(4242, true);
  const file = packing.pack(pixels, 208, 90);
  assert.ok(file.length < pixels.length / 2);
  assert.deepEqual(packing.unpack(file), { pixels, width: 208, height: 90 });
});
