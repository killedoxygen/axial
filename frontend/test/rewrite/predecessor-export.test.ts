import assert from 'node:assert/strict';
import test from 'node:test';
import { exportBrowserPreferences } from '../../../legacy/frontend/src/preferences-export';

function storage(values: Record<string, string> = {}) {
  return { getItem: (key: string): string | null => values[key] ?? null };
}

test('predecessor export reads only the two browser preference keys', () => {
  const reads: string[] = [];
  const values = new Map([
    ['axial_ui', '{"theme":"birch","sounds":false}'],
    ['axial:route', '{"name":"settings"}'],
    ['account-token', 'private-token'],
    ['axial_rewrite_ui', '{"theme":"nether"}'],
    ['axial-rewrite:route', '{"name":"home"}'],
  ]);
  const exported = exportBrowserPreferences({
    getItem(key) {
      reads.push(key);
      assert.ok(key === 'axial_ui' || key === 'axial:route');
      return values.get(key) ?? null;
    },
  });
  assert.deepEqual(reads, ['axial_ui', 'axial:route']);
  assert.deepEqual(JSON.parse(exported), {
    format: 'axial-browser-preferences',
    version: 1,
    preferences: { theme: 'birch', sounds: false },
    route: { name: 'settings' },
  });
  assert.ok(!exported.includes('private-token'));
});

test('predecessor export supplies defaults only for absent storage keys', () => {
  assert.deepEqual(JSON.parse(exportBrowserPreferences(storage())), {
    format: 'axial-browser-preferences',
    version: 1,
    preferences: {},
    route: null,
  });
  assert.equal(JSON.parse(exportBrowserPreferences(storage({ axial_ui: 'null' }))).preferences, null);
  assert.deepEqual(JSON.parse(exportBrowserPreferences(storage({ 'axial:route': '{}' }))).route, {});
});

test('predecessor export preserves unknown fields and values for import validation', () => {
  const preferences =
    '{"theme":"unknown-theme","future":{"items":[false,0,null,"日本語 🎮"]},"__proto__":{"value":true}}';
  const route = '{"name":"future-page","extension":{"enabled":false}}';
  const exported = JSON.parse(exportBrowserPreferences(storage({ axial_ui: preferences, 'axial:route': route })));
  assert.deepEqual(exported.preferences, JSON.parse(preferences));
  assert.deepEqual(exported.route, JSON.parse(route));
  assert.ok(Object.prototype.hasOwnProperty.call(exported.preferences, '__proto__'));
});

test('predecessor export bounds UTF-8 storage bytes before parsing', () => {
  const oversized = 'é'.repeat(512 * 1024);
  assert.ok(oversized.length < 1024 * 1024);
  for (const key of ['axial_ui', 'axial:route']) {
    assert.throws(() => exportBrowserPreferences(storage({ [key]: `"${oversized}"` })), /exceeds 1 MiB/);
    assert.throws(() => exportBrowserPreferences(storage({ [key]: `${oversized}é` })), /exceeds 1 MiB/);
  }
});

test('predecessor export includes envelope and Unicode bytes in its exact output limit', () => {
  const limit = 1024 * 1024;
  const encoder = new TextEncoder();
  const emptyBytes = encoder.encode(exportBrowserPreferences(storage({ axial_ui: '{"extension":""}' }))).byteLength;
  const available = limit - emptyBytes;
  const value = '🎮'.repeat(Math.floor(available / 4)) + 'x'.repeat(available % 4);
  const atLimit = exportBrowserPreferences(storage({ axial_ui: JSON.stringify({ extension: value }) }));
  assert.equal(encoder.encode(atLimit).byteLength, limit);
  assert.equal(JSON.parse(atLimit).preferences.extension, value);

  const oversized = JSON.stringify({ extension: `${value}é` });
  assert.ok(encoder.encode(oversized).byteLength < limit);
  assert.throws(() => exportBrowserPreferences(storage({ axial_ui: oversized })), /exceeds 1 MiB/);
});

test('predecessor export surfaces invalid JSON in either key without exposing stored text', () => {
  for (const key of ['axial_ui', 'axial:route']) {
    for (const malformed of ['', '{', 'private-value']) {
      assert.throws(() => exportBrowserPreferences(storage({ [key]: malformed })), {
        message: 'Stored browser preferences contain invalid JSON.',
      });
    }
  }
});

test('predecessor export surfaces storage failures without exposing exception details', () => {
  for (const failingKey of ['axial_ui', 'axial:route']) {
    assert.throws(
      () =>
        exportBrowserPreferences({
          getItem(key) {
            if (key === failingKey) throw new Error('private storage detail');
            return null;
          },
        }),
      { message: 'Stored browser preferences could not be read.' },
    );
  }
});
