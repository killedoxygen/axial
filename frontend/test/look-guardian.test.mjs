import assert from 'node:assert/strict';
import test from 'node:test';

import { harmoniousTilePalette } from '../src/ui/look-guardian.ts';
import { buildTheme } from '../src/tokens.ts';

const theme = buildTheme({ hue: 140 });

test('preserves an ordinary sampled palette', () => {
  assert.deepEqual(harmoniousTilePalette(theme, 42), [69, 83, 96, 187, 198, 204, 211]);
});

test('uses a deterministic evenly spaced fallback after bounded sampling is exhausted', () => {
  const first = harmoniousTilePalette(theme, 2);
  const second = harmoniousTilePalette(theme, 2);

  assert.deepEqual(first, second);
  assert.equal(first.length, 7);
  assert.deepEqual(first, [178, 185, 191, 198, 205, 212, 219]);
});
