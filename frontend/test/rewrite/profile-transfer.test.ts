import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import type { PreferenceProfile, PreferenceStorage } from '../../src/profile-transfer';

const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const ts: typeof import('typescript') = createRequire(resolve(frontend, 'package.json'))('typescript');
function source<T>(path: string, imports: Record<string, unknown> = {}, globals: Record<string, unknown> = {}): T {
  const filename = resolve(frontend, path);
  const exports = {};
  const compiled = ts.transpileModule(readFileSync(filename, 'utf8'), {
    fileName: filename,
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
  });
  vm.runInNewContext(
    compiled.outputText,
    {
      exports,
      Error,
      TextEncoder,
      require(id: string) {
        if (id === '@preact/signals') return { signal: <T>(value: T) => ({ value }) };
        if (Object.prototype.hasOwnProperty.call(imports, id)) return imports[id];
        throw new Error(`Unreviewed preference dependency: ${id}`);
      },
      ...globals,
    },
    { filename },
  );
  return exports as T;
}
const preferences = source<typeof import('../../src/preferences/local')>('src/preferences/local.ts');
const defaults = source<typeof import('../../src/default-skins')>('src/default-skins.ts');
const transfer = source<typeof import('../../src/profile-transfer')>('src/profile-transfer.ts', {
  './preferences/local': preferences,
  './default-skins': defaults,
});
const oldInstance = '0000000000000001';
const newInstance = '94d4ec3a-4a90-4d70-9e97-a5774f1c0a8a';
const texture = 'e'.repeat(64);
const account = '01234567-89ab-cdef-0123-456789abcdef';
function profile(): PreferenceProfile {
  return {
    format: 'axial-browser-preferences',
    version: 1,
    preferences: {
      theme: 'custom',
      customHue: 213,
      customVibrancy: 81,
      lightness: 22,
      sounds: false,
      hideSkinNametag: true,
      selectedSkin: `saved:${texture}`,
      selectedSkinsByAccount: { 'account:microsoft-legacy': 'default:alex', 'account:fallback': `saved:${texture}` },
      shortcuts: { 'launch-selected': { key: 'P', ctrl: true, shift: false, alt: true, meta: false } },
      overlayPositions: { 'command-center': { x: 17, y: 25, scaleX: 0.3, scaleY: 0.6 } },
      lastUpdateCheckAt: '2026-09-01T00:00:00Z',
      dismissedUpdateVersion: '1.2.3',
    },
    route: { name: 'content', id: 'modrinth:project', target: oldInstance },
  };
}
function storage() {
  const values = new Map<string, string>([
    ['axial_rewrite_ui', '{"theme":"birch"}'],
    ['axial-rewrite:route', '{"name":"settings"}'],
    ['axial_ui', JSON.stringify(profile().preferences)],
    ['axial:route', JSON.stringify(profile().route)],
  ]);
  let beforeWrite: ((key: string, value: string | null) => boolean) | null = null;
  const writes: Array<[string, string | null]> = [];
  const api: PreferenceStorage = {
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => {
      writes.push([key, value]);
      if (beforeWrite?.(key, value) === false) return;
      values.set(key, value);
    },
    removeItem: (key) => {
      writes.push([key, null]);
      if (beforeWrite?.(key, null) === false) return;
      values.delete(key);
    },
  };
  return {
    api,
    values,
    writes,
    setWriter: (run: typeof beforeWrite) => {
      beforeWrite = run;
    },
  };
}
const bindings = {
  accounts: { 'Microsoft-Legacy': account },
  instances: { [oldInstance]: newInstance },
  skins: [texture],
  currentAccounts: [account],
  currentSkins: [texture],
};

test('the actual predecessor exporter preserves every field and uses only the two predecessor keys', () => {
  const exporter = source<typeof import('../../../legacy/frontend/src/preferences-export')>(
    '../legacy/frontend/src/preferences-export.ts',
  );
  const h = storage();
  const reads: string[] = [];
  const exported = exporter.exportBrowserPreferences({
    getItem: (key) => {
      reads.push(key);
      return h.api.getItem(key);
    },
  });
  const parsed = transfer.previewPreferenceImport(exported);
  assert.deepEqual(JSON.parse(JSON.stringify(parsed)), profile());
  assert.deepEqual(reads, ['axial_ui', 'axial:route']);
  assert.equal(h.writes.length, 0);
  const mapped = transfer.resolvePreferenceProfile(parsed, bindings);
  transfer.importPreferenceProfile(JSON.stringify(mapped), h.api);
  assert.equal(h.api.getItem('axial_ui'), JSON.stringify(profile().preferences));
  assert.equal(
    JSON.parse(h.api.getItem('axial_rewrite_ui')!).selectedSkinsByAccount[`account:${account}`],
    'default:alex',
  );
  assert.equal(JSON.parse(h.api.getItem('axial-rewrite:route')!).target, newInstance);
  assert.equal(JSON.parse(h.api.getItem('axial_rewrite_ui')!).customHue, 213);
});

test('mapping preserves all other values, fallback selections, built-ins and provider content IDs', () => {
  for (const route of [
    { name: 'instance', id: oldInstance },
    { name: 'discover', target: oldInstance },
    { name: 'content', id: 'curseforge:123', target: oldInstance },
  ] as const) {
    const input = { ...profile(), route };
    const mapped = transfer.resolvePreferenceProfile(input, bindings);
    assert.equal(
      JSON.stringify({ ...mapped.preferences, selectedSkinsByAccount: input.preferences.selectedSkinsByAccount }),
      JSON.stringify(input.preferences),
    );
    assert.equal(mapped.preferences.selectedSkinsByAccount['account:fallback'], `saved:${texture}`);
    assert.equal(mapped.preferences.selectedSkinsByAccount['account:microsoft-legacy'], undefined);
    assert.equal(
      JSON.stringify(mapped.route),
      JSON.stringify(route.name === 'instance' ? { ...route, id: newInstance } : { ...route, target: newInstance }),
    );
  }
  const input = profile();
  input.preferences.selectedSkin = '  ';
  input.preferences.selectedSkinsByAccount = { 'account:fallback': 'default:steve' };
  input.route = { name: 'content', id: 'provider:unchanged' };
  assert.equal(
    JSON.stringify(transfer.preferenceReferences(input)),
    JSON.stringify({ accounts: false, skins: false, instances: false }),
  );
  assert.equal(
    JSON.stringify(
      transfer.resolvePreferenceProfile(input, {
        accounts: null,
        instances: {},
        skins: [],
        currentAccounts: [],
        currentSkins: [],
      }),
    ),
    JSON.stringify(input),
  );
});

test('missing mappings, removed references and ambiguous account keys block rather than dropping preferences', () => {
  const changes: Array<Partial<Parameters<typeof transfer.resolvePreferenceProfile>[1]>> = [
    { accounts: null },
    { accounts: {} },
    { currentAccounts: [] },
    { currentSkins: [] },
    { skins: [] },
    { instances: {} },
    { accounts: { 'Microsoft-Legacy': account, 'microsoft-legacy': account } },
  ];
  for (const change of changes)
    assert.throws(() => transfer.resolvePreferenceProfile(profile(), { ...bindings, ...change }));
  const input = profile();
  input.preferences.selectedSkin = 'default:missing';
  assert.throws(() => transfer.resolvePreferenceProfile(input, bindings));
  input.preferences.selectedSkin = 'https://untrusted.invalid/skin';
  assert.throws(() => transfer.resolvePreferenceProfile(input, bindings));
  assert.throws(
    () =>
      transfer.resolvePreferenceProfile(profile(), {
        ...bindings,
        accounts: { 'Microsoft-Legacy': 'fallback' },
        currentAccounts: ['fallback'],
      }),
    /collide/,
  );
});

test('invalid exports fail before storage writes, including UTF-8 size and unknown retained values', () => {
  const h = storage();
  for (const value of [
    '{',
    JSON.stringify({ ...profile(), version: 2 }),
    JSON.stringify({ ...profile(), preferences: { ...profile().preferences, forgotten: true } }),
    JSON.stringify({ ...profile(), preferences: { ...profile().preferences, lightness: Infinity } }),
    JSON.stringify({ ...profile(), route: { name: 'instance', id: '' } }),
    JSON.stringify({ ...profile(), preferences: { ...profile().preferences, selectedSkin: 'é'.repeat(600_000) } }),
    '{"format":"axial-browser-preferences","version":1,"preferences":{"__proto__":{}},"route":null}',
  ])
    assert.throws(() => transfer.importPreferenceProfile(value, h.api));
  assert.equal(h.writes.length, 0);
});

test('synchronous write, verification and reload failures restore both keys or report incomplete rollback', () => {
  for (const mode of ['throw', 'ignore', 'reload', 'rollback'] as const) {
    const h = storage();
    const before = [...h.values];
    let write = 0;
    h.setWriter(() => {
      write += 1;
      if (write === 2 && mode === 'ignore') return false;
      if ((write === 2 && mode !== 'reload') || (mode === 'rollback' && write > 2))
        throw new Error('Storage unavailable');
      return true;
    });
    let failure: unknown;
    try {
      transfer.importPreferenceProfile(JSON.stringify(profile()), h.api, () => {
        if (mode === 'reload') throw new Error('Reload blocked');
      });
    } catch (error) {
      failure = error;
    }
    assert.ok(failure instanceof Error);
    assert.equal(transfer.preferenceImportNeedsReload(failure), mode === 'rollback');
    if (mode !== 'rollback') assert.deepEqual([...h.values], before);
  }
});

test('snapshot witnesses cover live and stored preferences/routes independently', () => {
  const h = storage();
  const prefs = preferences.defaultLocalPreferences();
  const route = { name: 'settings' as const };
  const baseline = transfer.preferenceSnapshot(h.api, prefs, route);
  prefs.sounds = false;
  assert.notEqual(transfer.preferenceSnapshot(h.api, prefs, route), baseline);
  prefs.sounds = true;
  assert.notEqual(transfer.preferenceSnapshot(h.api, prefs, { name: 'home' }), baseline);
  h.values.set('axial_rewrite_ui', '{"theme":"end"}');
  assert.notEqual(transfer.preferenceSnapshot(h.api, prefs, route), baseline);
  h.values.set('axial_rewrite_ui', '{"theme":"birch"}');
  h.values.set('axial-rewrite:route', '{"name":"home"}');
  assert.notEqual(transfer.preferenceSnapshot(h.api, prefs, route), baseline);
});

test('the two real write-owner fences survive reload request and can resume after a restored failure', () => {
  const h = storage();
  const browserPreferenceImports = {
    './preferences/local': preferences,
    './native': { hasNativeDesktopRuntime: () => false },
    './preferences/persistence': { canEditPreferences: () => true },
  };
  const state = source<typeof import('../../src/state')>(
    'src/state.ts',
    browserPreferenceImports,
    { localStorage: h.api },
  );
  const ui = source<typeof import('../../src/ui-state')>(
    'src/ui-state.ts',
    browserPreferenceImports,
    { localStorage: h.api, document: { querySelector: () => null } },
  );
  const resumeLocal = state.suspendLocalStatePersistence();
  const resumeRoute = ui.suspendRoutePersistence();
  const imported = profile();
  let importedBytes: string | null = null;
  transfer.importPreferenceProfile(JSON.stringify(imported), h.api, () => {
    importedBytes = h.api.getItem('axial_rewrite_ui');
    assert.ok(importedBytes);
    assert.deepEqual(JSON.parse(importedBytes), imported.preferences);
    state.local.theme = 'end';
    state.saveLocalState();
    ui.navigate({ name: 'home' });
    assert.equal(h.api.getItem('axial_rewrite_ui'), importedBytes);
    assert.equal(h.api.getItem('axial-rewrite:route'), JSON.stringify(imported.route));
  });
  state.saveLocalState();
  ui.navigate({ name: 'settings' });
  assert.equal(h.api.getItem('axial_rewrite_ui'), importedBytes);
  assert.equal(h.api.getItem('axial-rewrite:route'), JSON.stringify(imported.route));
  resumeRoute();
  resumeLocal();
  state.saveLocalState();
  ui.navigate({ name: 'accounts' });
  assert.equal(JSON.parse(h.api.getItem('axial_rewrite_ui')!).theme, 'end');
  assert.equal(JSON.parse(h.api.getItem('axial-rewrite:route')!).name, 'accounts');
});
