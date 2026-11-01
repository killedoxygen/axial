import assert from 'node:assert/strict';
import test from 'node:test';

import { comboMatches, effectiveCombos, shortcutById, shortcutHint } from '../src/shortcuts';

function onPlatform(platform: string, run: () => void): void {
  const original = Object.getOwnPropertyDescriptor(globalThis, 'navigator');
  Object.defineProperty(globalThis, 'navigator', { value: { platform, userAgent: platform }, configurable: true });
  try {
    run();
  } finally {
    if (original) Object.defineProperty(globalThis, 'navigator', original);
    else delete (globalThis as { navigator?: unknown }).navigator;
  }
}

const key = (init: KeyboardEventInit & { key: string }): KeyboardEvent =>
  ({ ctrlKey: false, shiftKey: false, altKey: false, metaKey: false, ...init }) as KeyboardEvent;

test('built-in shortcuts use Cmd and macOS symbols on a Mac', () => {
  onPlatform('MacIntel', () => {
    const settings = shortcutById('open-settings');
    assert.deepEqual(effectiveCombos(settings), [{ key: ',', meta: true }]);
    assert.equal(shortcutHint('open-settings'), '⌘,');
    const [combo] = effectiveCombos(settings);
    assert.ok(comboMatches(combo!, key({ key: ',', metaKey: true })));
    assert.ok(!comboMatches(combo!, key({ key: ',', ctrlKey: true })));
    assert.deepEqual(effectiveCombos(shortcutById('dismiss')), [{ key: 'Escape' }]);
  });
});

test('built-in shortcuts keep Ctrl elsewhere', () => {
  onPlatform('Win32', () => {
    const [combo] = effectiveCombos(shortcutById('new-instance'));
    assert.deepEqual(combo, { key: 'n', ctrl: true });
    assert.equal(shortcutHint('new-instance'), 'Ctrl N');
    assert.ok(comboMatches(combo!, key({ key: 'n', ctrlKey: true })));
  });
});
