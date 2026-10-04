import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import * as contract from '../../src/dto-contract';
import * as preferences from '../../src/preferences/local';
import type { Config } from '../../src/types-settings';
import type { InterfacePreferences } from '../../src/generated/InterfacePreferences';
import type { InterfacePreferencesSnapshot } from '../../src/generated/InterfacePreferencesSnapshot';
import type { InterfacePreferencesUpdate } from '../../src/generated/InterfacePreferencesUpdate';
import type { NativePreferencesRequest } from '../../src/native';

const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const ts: typeof import('typescript') = createRequire(resolve(frontend, 'package.json'))('typescript');
const tick = (): Promise<void> => new Promise((done) => setImmediate(done));
function signal<T>(value: T) {
  return { value };
}

function source<T>(path: string, imports: Record<string, unknown>, globals: Record<string, unknown> = {}): T {
  const filename = resolve(frontend, 'src', path);
  const compiled = ts.transpileModule(readFileSync(filename, 'utf8'), {
    fileName: filename,
    compilerOptions: {
      target: ts.ScriptTarget.ES2020,
      module: ts.ModuleKind.CommonJS,
      jsx: ts.JsxEmit.ReactJSX,
      jsxImportSource: 'preact',
    },
  });
  const exports = {};
  vm.runInNewContext(
    compiled.outputText,
    {
      exports,
      Error,
      URLSearchParams,
      require(id: string): unknown {
        if (id === '@preact/signals') return { signal };
        if (Object.prototype.hasOwnProperty.call(imports, id)) return imports[id];
        throw new Error(`Unreviewed preference dependency: ${id}`);
      },
      ...globals,
    },
    { filename },
  );
  return exports as T;
}

function deferred<T>() {
  let resolveValue!: (value: T) => void;
  let reject!: (value: unknown) => void;
  const promise = new Promise<T>((yes, no) => {
    resolveValue = yes;
    reject = no;
  });
  return { promise, resolve: resolveValue, reject };
}

function config(patch: Partial<Config> = {}): Config {
  return {
    revision: 0,
    account_selection_revision: 0,
    username: 'Player',
    launch_auth_mode: 'offline',
    max_memory_mb: 4096,
    min_memory_mb: 1024,
    java_path_override: '',
    window_width: 854,
    window_height: 480,
    jvm_preset: '',
    performance_mode: 'vanilla',
    theme: '',
    custom_hue: null,
    custom_vibrancy: null,
    lightness: null,
    onboarding_done: true,
    telemetry_enabled: false,
    discord_rpc_enabled: false,
    discord_rpc_onboarding_seen: false,
    music_enabled: false,
    music_volume: 0.5,
    music_track: 0,
    ...patch,
  };
}

function envelope(patch: Partial<InterfacePreferences['preferences']> = {}): InterfacePreferences {
  return {
    version: 1,
    preferences: { ...preferences.defaultLocalPreferences(), ...patch },
    route: { name: 'settings' },
  };
}

function harness(
  options: { native?: boolean; value?: InterfacePreferences | null; pending?: NativePreferencesRequest } = {},
) {
  const nativeEnabled = options.native !== false;
  const backend = {
    snapshot: {
      revision: 4,
      value: options.value === undefined ? envelope() : options.value,
    } as InterfacePreferencesSnapshot,
  };
  const writes: InterfacePreferencesUpdate[] = [];
  const reads: string[] = [];
  const events: string[] = [];
  const completions: Array<{ requestId: string; saved: boolean }> = [];
  const notices: string[] = [];
  const storage = new Map([
    ['axial_rewrite_ui', JSON.stringify({ theme: 'nether', sounds: false })],
    ['axial-rewrite:route', JSON.stringify({ name: 'accounts' })],
  ]);
  let storageReads = 0;
  let reloads = 0;
  let listener: ((event: { payload: unknown }) => void) | undefined;
  let pending = options.pending ?? null;
  let readPending: () => Promise<unknown> = async () => pending;
  let read: () => Promise<unknown> = async () => structuredClone(backend.snapshot);
  function commit(update: InterfacePreferencesUpdate): unknown {
    assert.equal(update.expected_revision, backend.snapshot.revision);
    const current = backend.snapshot.value ?? {
      version: 1,
      preferences: preferences.defaultLocalPreferences(),
      route: null,
    };
    const change = update.change;
    const value =
      change.kind === 'replace'
        ? change.value
        : change.kind === 'local'
          ? { ...current, preferences: change.preferences }
          : { ...current, route: change.route };
    backend.snapshot = { revision: update.expected_revision + 1, value: structuredClone(value) };
    return { revision: backend.snapshot.revision };
  }
  let write: (update: InterfacePreferencesUpdate) => Promise<unknown> = async (update) => commit(update);
  const api = {
    api: async (method: string, path: string, body?: unknown): Promise<unknown> => {
      if (method === 'GET') {
        reads.push(path);
        return read();
      }
      assert.equal(path, '/config/interface-preferences');
      assert.equal(method, 'PUT');
      const update = body as InterfacePreferencesUpdate;
      writes.push(structuredClone(update));
      return write(update);
    },
    isApiError: (error: unknown) => error instanceof Error && 'status' in error,
    apiResourceUrl: (path: string) => path,
  };
  const nativeWindow = {
    __TAURI__: {
      core: {
        invoke: async (command: string, args?: Record<string, unknown>) => {
          events.push(command);
          if (command === 'pending_interface_preferences') return readPending();
          if (command === 'complete_interface_preferences') {
            completions.push(structuredClone(args) as { requestId: string; saved: boolean });
            return;
          }
          if (command === 'window_set_resize_background') return;
          if (command === 'app_version') return '1.0.0';
          throw new Error(`Unexpected native command ${command}`);
        },
      },
      event: {
        listen: async (name: string, callback: typeof listener) => {
          events.push(`listen:${name}`);
          listener = callback;
          return () => {};
        },
      },
    },
  };
  const native = source<typeof import('../../src/native')>(
    'native.ts',
    { './dto-contract': contract },
    { window: nativeEnabled ? nativeWindow : {} },
  );
  const owner = source<typeof import('../../src/preferences/persistence')>(
    'preferences/persistence.ts',
    {
      '../api': api,
      '../dto-contract': contract,
      '../native': native,
      '../hooks/use-autosave': { prepareAutoSaves: () => ({ done: Promise.resolve(true), release() {} }) },
      '../toast': { toast: (message: string) => notices.push(message) },
      './local': preferences,
    },
    {
      location: {
        reload() {
          reloads++;
        },
      },
    },
  );
  const localStorage = {
    getItem(key: string) {
      storageReads++;
      return storage.get(key) ?? null;
    },
    setItem(key: string, value: string) {
      storage.set(key, value);
    },
    removeItem(key: string) {
      storage.delete(key);
    },
  };
  const state = source<typeof import('../../src/state')>(
    'state.ts',
    {
      './preferences/local': preferences,
      './preferences/persistence': owner,
      './native': native,
    },
    { localStorage },
  );
  const ui = source<typeof import('../../src/ui-state')>(
    'ui-state.ts',
    {
      './preferences/local': preferences,
      './preferences/persistence': owner,
      './native': native,
    },
    { localStorage },
  );
  const soundCalls: string[] = [];
  const Sound = {
    ui: (value: string) => soundCalls.push(value),
    enabled: true,
    warmup: async () => {
      soundCalls.push(`warmup:${Sound.enabled}`);
    },
  };
  const configWrites: unknown[] = [];
  const configState = signal(config());
  const css = new Map<string, string>();
  const theme = source<typeof import('../../src/theme')>(
    'theme.ts',
    {
      './state': state,
      './hooks/use-autosave': {
        saveConfigPatch: async (value: unknown) => {
          configWrites.push(value);
        },
      },
      './store': { config: configState },
      './sound': { Sound },
      './tokens': { buildTheme: (value: unknown) => value },
      './toast': { toast: (message: string) => notices.push(message) },
      './native': native,
      './preferences/persistence': owner,
    },
    {
      document: {
        documentElement: {
          style: { setProperty: (key: string, value: string) => css.set(key, value) },
          setAttribute() {},
        },
      },
    },
  );
  const shortcuts = source<typeof import('../../src/shortcuts')>('shortcuts.ts', {
    './ui-state': ui,
    './store': {},
    './actions': {},
    './launch': {},
    './sound': { Sound },
    './state': state,
  });
  const skin = source<typeof import('../../src/player-skin')>('player-skin.ts', {
    './api': api,
    './default-skins': {
      DEFAULT_SKINS: [
        { id: 'steve', src: 'steve.png' },
        { id: 'alex', src: 'alex.png' },
      ],
    },
    './machines/accounts-state': {
      accountsSnapshot: signal({ state: 'loading', accounts: [], status: null }),
      activeAccount: () => null,
    },
    './state': state,
    './store': { config: signal(config()) },
  });
  return {
    owner,
    native,
    state,
    ui,
    theme,
    shortcuts,
    skin,
    backend,
    writes,
    reads,
    events,
    completions,
    notices,
    Sound,
    configState,
    storage,
    css,
    soundCalls,
    configWrites,
    commit,
    storageReads: () => storageReads,
    reloads: () => reloads,
    read(next: typeof read): void {
      read = next;
    },
    write(next: typeof write): void {
      write = next;
    },
    readPending(next: typeof readPending): void {
      readPending = next;
    },
    async hydrate(cfg = config()): Promise<void> {
      const loaded = await owner.initializeNativePreferences(cfg);
      Object.assign(state.local, loaded.preferences);
      ui.route.value = loaded.route ?? { name: 'home' };
    },
    emit(phase: NativePreferencesRequest['phase'], id = 'preferences-1'): void {
      assert.ok(listener);
      pending = { request_id: id, phase };
      listener({ payload: pending });
    },
  };
}

test('native hydration ignores shared WebView storage and preserves explicit obsidian and fractional values', async () => {
  const h = harness({
    value: envelope({ theme: 'obsidian', customHue: 142.75, customVibrancy: 77.25, lightness: 0.5 }),
  });
  h.ui.restoreRoute();
  h.shortcuts.setShortcutOverride('new-instance', { key: 'x', meta: true });
  h.skin.setSelectedSkin('default:alex');
  h.theme.applyTheme('custom', 44.5);
  assert.equal(h.writes.length, 0);
  assert.equal(h.storageReads(), 0);
  assert.equal(h.state.local.theme, 'obsidian');
  await h.hydrate(config({ theme: 'nether' }));
  h.theme.applyConfigTheme(config({ theme: 'nether' }));
  assert.equal(h.state.local.theme, 'obsidian');
  assert.equal(h.state.local.customHue, 142.75);
  assert.equal(h.ui.route.value.name, 'settings');
  assert.deepEqual(h.events.slice(0, 2), ['listen:axial:desktop:preferences', 'pending_interface_preferences']);
  h.theme.applyTheme('custom', 243.875, { vibrancy: 62.25, lightness: 1.5 });
  await h.owner.flushNativePreferences();
  assert.equal(h.backend.snapshot.value?.preferences.customHue, 243.875);
  assert.equal(h.backend.snapshot.value?.preferences.customVibrancy, 62.25);
  assert.equal(h.configWrites.length, 0);
  assert.equal(h.storageReads(), 0);
});

test('a missing native envelope uses config fallback without writing defaults, while browser storage remains unchanged', async () => {
  const h = harness({ value: null });
  await h.hydrate(config({ theme: 'end', custom_hue: 210, custom_vibrancy: 60 }));
  assert.equal(h.state.local.theme, 'end');
  assert.equal(h.ui.route.value.name, 'home');
  assert.equal(h.writes.length, 0);
  h.ui.navigate({ name: 'settings' });
  await h.owner.flushNativePreferences();
  assert.equal(h.backend.snapshot.value?.preferences.theme, 'end');
  const browser = harness({ native: false });
  assert.equal(browser.state.local.theme, 'nether');
  browser.ui.restoreRoute();
  assert.equal(browser.ui.route.value.name, 'accounts');
  browser.shortcuts.setShortcutOverride('new-instance', { key: 'n', meta: true });
  assert.equal(JSON.parse(browser.storage.get('axial_rewrite_ui')!).shortcuts['new-instance'].meta, true);
  assert.equal(browser.writes.length, 0);
});

test('one in-flight write coalesces later local and route edits against the latest acknowledgement', async () => {
  const h = harness();
  await h.hydrate();
  const held = deferred<unknown>();
  h.write((update) => (h.writes.length === 1 ? held.promise : Promise.resolve(h.commit(update))));
  h.shortcuts.setShortcutOverride('new-instance', { key: 'x', meta: true });
  await tick();
  for (let index = 0; index < 30; index++) {
    h.skin.setSelectedSkin(index % 2 ? 'default:alex' : 'default:steve');
    h.ui.navigate({ name: 'instance', id: `instance-${index}` });
  }
  assert.equal(h.writes.length, 1);
  held.resolve(h.commit(h.writes[0]));
  await h.owner.flushNativePreferences();
  assert.equal(h.writes.length, 2);
  assert.deepEqual(
    h.writes.map((write) => write.expected_revision),
    [4, 5],
  );
  assert.equal(h.backend.snapshot.value?.route?.name, 'instance');
  assert.equal((h.backend.snapshot.value?.route as { id: string }).id, 'instance-29');
  assert.equal(h.backend.snapshot.value?.preferences.selectedSkin, 'default:alex');
});

test('lost responses reconcile sorted maps exactly and an older read never replays the mutation', async () => {
  const h = harness();
  await h.hydrate();
  h.read(async () => {
    const snapshot = structuredClone(h.backend.snapshot);
    if (snapshot.value) {
      const local = snapshot.value.preferences;
      // Model the backend's BTreeMap response independently of submitted order.
      local.shortcuts = Object.fromEntries(Object.entries(local.shortcuts).sort(([a], [b]) => a.localeCompare(b)));
      local.overlayPositions = Object.fromEntries(
        Object.entries(local.overlayPositions).sort(([a], [b]) => a.localeCompare(b)),
      );
      local.selectedSkinsByAccount = Object.fromEntries(
        Object.entries(local.selectedSkinsByAccount).sort(([a], [b]) => a.localeCompare(b)),
      );
    }
    return snapshot;
  });
  h.write(async (update) => {
    h.commit(update);
    throw new Error('Response lost');
  });
  h.state.local.shortcuts = { z: { key: 'z', meta: true }, a: { key: 'a', ctrl: true } };
  h.state.local.overlayPositions = { z: { x: 3.75, y: 4.5, scaleX: 0.3 }, a: { x: 1, y: 2 } };
  h.state.local.selectedSkinsByAccount = { z: 'default:alex', a: 'default:steve' };
  h.state.saveLocalState();
  await h.owner.flushNativePreferences();
  assert.equal(h.writes.length, 1);
  assert.equal(h.notices.length, 0);
  h.write(async () => {
    throw new Error('Response not yet committed');
  });
  h.skin.setSelectedSkin('default:alex');
  await assert.rejects(h.owner.flushNativePreferences());
  await assert.rejects(h.owner.flushNativePreferences());
  assert.equal(h.writes.length, 2);
  assert.equal(h.state.local.selectedSkin, 'default:alex');
  assert.ok(h.notices.length > 0);
  h.commit(h.writes[1]);
  await h.owner.flushNativePreferences();
  assert.equal(h.writes.length, 2);
});

test('flush seals real writers and navigation until matching release, and supersedes coalesced native requests', async () => {
  const h = harness();
  await h.hydrate();
  h.ui.navigate({ name: 'accounts' });
  await h.owner.flushNativePreferences();
  h.emit('flush');
  const before = JSON.stringify(h.state.local);
  h.shortcuts.setShortcutOverride('new-instance', { key: 'x', ctrl: true });
  h.skin.setSelectedSkin('default:alex');
  h.theme.applyTheme('end', null);
  h.theme.applyTheme('custom', 18, { silent: true, transient: true });
  h.ui.navigate({ name: 'downloads' });
  h.ui.goBack();
  assert.equal(JSON.stringify(h.state.local), before);
  assert.equal(h.ui.route.value.name, 'accounts');
  assert.equal(h.css.size, 0);
  h.emit('flush', 'preferences-2');
  h.emit('release', 'preferences-1');
  h.emit('discard', 'preferences-1');
  assert.equal(h.owner.canEditPreferences(), false);
  await tick();
  assert.deepEqual(h.completions, [{ requestId: 'preferences-2', saved: true }]);
  h.emit('release', 'preferences-2');
  assert.equal(h.owner.canEditPreferences(), true);
  h.ui.goBack();
  assert.equal(h.ui.route.value.name, 'settings');
  await h.owner.flushNativePreferences();
});

test('discard seals queued drafts without saving them and release recovers them after the in-flight write', async () => {
  const h = harness();
  await h.hydrate();
  const held = deferred<unknown>();
  h.write((update) => (h.writes.length === 1 ? held.promise : Promise.resolve(h.commit(update))));
  h.shortcuts.setShortcutOverride('new-instance', { key: 'q', meta: true });
  await tick();
  h.skin.setSelectedSkin('default:alex');
  h.emit('discard');
  h.skin.setSelectedSkin('default:steve');
  held.resolve(h.commit(h.writes[0]));
  await tick();
  assert.equal(h.writes.length, 1);
  assert.equal(h.state.local.selectedSkin, 'default:alex');
  assert.deepEqual(h.completions, [{ requestId: 'preferences-1', saved: true }]);
  h.emit('release');
  await h.owner.flushNativePreferences();
  assert.equal(h.backend.snapshot.value?.preferences.selectedSkin, 'default:alex');
  assert.equal(h.writes.length, 2);
});

test('a refused save remains a draft and native release does not replay it', async () => {
  const h = harness();
  await h.hydrate();
  h.write(async () => {
    throw Object.assign(new Error('Conflict'), { status: 409 });
  });
  h.skin.setSelectedSkin('default:alex');
  await assert.rejects(h.owner.flushNativePreferences());
  h.emit('flush');
  await tick();
  const count = h.writes.length;
  assert.equal(h.completions[h.completions.length - 1].saved, false);
  h.emit('release');
  await tick();
  assert.equal(h.writes.length, count);
  assert.equal(h.state.local.selectedSkin, 'default:alex');
});

test('bootstrap retains its visible retry state until native hydration succeeds before theme, sound and deferred writers', async () => {
  const h = harness({ value: envelope({ theme: 'obsidian', customHue: 125.25, sounds: false }) });
  h.Sound.enabled = false;
  h.read(async () => {
    throw new Error('Profile preferences unavailable');
  });
  const store = {
    config: h.configState,
    appVersion: signal(''),
    bootstrapError: signal<string | null>(null),
    bootstrapState: signal('loading'),
    devMode: signal(false),
    instances: signal([]),
    lastInstanceId: signal(null),
    launchSessions: signal({}),
    systemInfo: signal(null),
    versions: signal([]),
  };
  const deferredCalls: string[] = [];
  const response: Record<string, unknown> = {
    '/config': config({ theme: 'nether' }),
    '/status': { dev_mode: false, setup_required: false },
    '/system': {},
    '/music/status': { count: 0 },
    '/versions': { versions: [] },
    '/instances': { instances: [], last_instance_id: null },
    '/launch/sessions': {},
  };
  const bootstrap = source<typeof import('../../src/bootstrap')>(
    'bootstrap.ts',
    {
      './api': {
        initializeApiBase: async () => {},
        api: async (_method: string, path: string) => {
          assert.ok(path in response);
          return response[path];
        },
      },
      './App': {
        preloadDeferredViews() {
          deferredCalls.push('views');
        },
      },
      './dto-contract': contract,
      './dto-core': Object.fromEntries(
        [
          'configResponse',
          'instancesResponse',
          'launcherStatusResponse',
          'musicStatusResponse',
          'systemInfoResponse',
          'versionsResponse',
        ].map((name) => [name, (value: unknown) => value]),
      ),
      './machines/downloads': { refreshInstallQueue: async () => {} },
      './launch': {},
      './launch-response-adapters': { launchSessionsResponse: (value: unknown) => value },
      './music': { Music: { setTrackCount() {}, applyConfig() {}, enabled: false } },
      './native': h.native,
      './preferences/persistence': h.owner,
      './state': h.state,
      './store': store,
      './sound': {
        Sound: h.Sound,
        bindButtonSounds() {
          deferredCalls.push('buttons');
        },
      },
      './player-skin': {
        refreshAccountSkin() {
          deferredCalls.push('skin');
          h.skin.refreshAccountSkin();
        },
      },
      './startup-warnings': { startupWarningMessages: () => [] },
      './theme': h.theme,
      './toast': { toast() {} },
      './ui-state': h.ui,
      './updater': {
        scheduleAutoUpdateCheck() {
          deferredCalls.push('updater');
        },
      },
      './utils': { errMessage: (error: Error) => error.message },
    },
    {
      window: {
        requestIdleCallback(run: () => void) {
          run();
        },
        addEventListener() {},
      },
    },
  );
  await bootstrap.startApplicationBootstrap();
  assert.equal(store.bootstrapState.value, 'error');
  assert.equal(store.bootstrapError.value, 'Profile preferences unavailable');
  assert.equal(h.owner.nativePreferencesHydrated(), false);
  assert.equal(h.css.size, 0);
  assert.deepEqual(h.soundCalls, []);
  assert.deepEqual(deferredCalls, []);
  assert.equal(h.writes.length, 0);
  const held = deferred<unknown>();
  h.read(() => held.promise);
  const retry = bootstrap.startApplicationBootstrap();
  assert.equal(bootstrap.startApplicationBootstrap(), retry);
  h.shortcuts.setShortcutOverride('new-instance', { key: 'x' });
  h.ui.navigate({ name: 'downloads' });
  assert.equal(h.writes.length, 0);
  assert.equal(h.ui.route.value.name, 'home');
  held.resolve(h.backend.snapshot);
  await retry;
  assert.equal(store.bootstrapState.value, 'ready');
  assert.equal(store.bootstrapError.value, null);
  assert.equal(h.state.local.theme, 'obsidian');
  assert.equal(h.state.local.customHue, 125.25);
  assert.equal(h.ui.route.value.name, 'settings');
  assert.equal(h.Sound.enabled, false);
  assert.deepEqual(h.soundCalls, ['warmup:false']);
  assert.deepEqual(deferredCalls, ['buttons', 'views', 'skin', 'updater']);
  assert.ok(h.css.size > 0);
  assert.equal(h.writes.length, 0);
  assert.equal(h.storageReads(), 0);
  assert.equal(h.events.filter((event) => event === 'listen:axial:desktop:preferences').length, 1);
});

test('listener events supersede stale pending IPC and compare native request identities without numeric rounding', async () => {
  const h = harness();
  const pending = deferred<unknown>();
  h.readPending(() => pending.promise);
  const hydration = h.hydrate();
  await tick();
  h.emit('discard', 'preferences-9007199254740993');
  pending.resolve({ request_id: 'preferences-9007199254740992', phase: 'flush' });
  await hydration;
  await tick();
  assert.equal(h.owner.canEditPreferences(), false);
  assert.deepEqual(h.completions, [{ requestId: 'preferences-9007199254740993', saved: true }]);
  h.emit('release', 'preferences-9007199254740992');
  h.emit('flush', 'preferences-9007199254740992');
  assert.equal(h.owner.canEditPreferences(), false);
  h.emit('release', 'preferences-9007199254740993');
  assert.equal(h.owner.canEditPreferences(), true);
  h.emit('discard', 'preferences-9007199254740993');
  assert.equal(h.owner.canEditPreferences(), true);
});

test('an already rendered audio control cannot mutate local sound or audio state through a native seal', async () => {
  const h = harness();
  await h.hydrate();
  type Node = { props: { children?: Node[]; title?: string; control?: { props: { onChange: () => void } } } };
  const jsx = (_type: unknown, props: Node['props']): Node => ({ props });
  const audio = source<typeof import('../../src/views/settings/AudioSection')>('views/settings/AudioSection.tsx', {
    'preact/jsx-runtime': { jsx, jsxs: jsx },
    'preact/hooks': { useState: (value: unknown) => [value, () => {}], useEffect() {} },
    '../../ui/Atoms': { Toggle: 'Toggle' },
    '../../ui/Slider': {},
    '../../ui/SettingsSheet': { SettingRow: 'SettingRow', SettingsSection: 'SettingsSection' },
    '../../state': h.state,
    '../../sound': { Sound: h.Sound },
    '../../music': { musicStateVersion: signal(0), Music: { enabled: false, volume: 50 } },
  });
  const node = audio.AudioSection() as unknown as Node;
  const toggle = node.props.children?.find((child) => child && child.props.title === 'UI sounds')?.props.control;
  assert.ok(toggle);
  h.emit('discard');
  toggle.props.onChange();
  assert.equal(h.state.local.sounds, true);
  assert.equal(h.Sound.enabled, true);
  assert.equal(h.writes.length, 0);
  h.emit('release');
  toggle.props.onChange();
  await h.owner.flushNativePreferences();
  assert.equal(h.backend.snapshot.value?.preferences.sounds, false);
  assert.equal(h.Sound.enabled, false);
});
