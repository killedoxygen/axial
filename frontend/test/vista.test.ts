import assert from 'node:assert/strict';
import test from 'node:test';

import { paintedTextures } from '../src/mock/block-textures';
import { createRenderer } from '../src/vista/render';
import { buildWorld } from '../src/vista/world';

const pack = paintedTextures();

function picture(seed: number, dark: boolean, width = 208, height = 90): Uint8ClampedArray {
  const renderer = createRenderer(buildWorld(seed, dark, pack)!, width, height);
  renderer.columns(0, width);
  return renderer.finish();
}

test('a seed always draws the same world, and columns may be drawn in any batches', () => {
  const whole = picture(4242, true);
  const renderer = createRenderer(buildWorld(4242, true, pack)!, 208, 90);
  renderer.columns(120, 208);
  renderer.columns(0, 64);
  renderer.columns(64, 120);
  assert.deepEqual(renderer.finish(), whole);
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
