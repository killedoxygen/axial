import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import * as contract from '../../src/dto-contract';
import * as dto from '../../src/dto-core';
import * as preferences from '../../src/preferences/local';
import type { Config } from '../../src/types-settings';
import type { EnrichedInstance } from '../../src/types-instance';
import type { NativePreferencesRequest } from '../../src/native';

const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const dependencies = createRequire(resolve(frontend, 'package.json'));
const ts: typeof import('typescript') = dependencies('typescript');
const signals: typeof import('@preact/signals') = dependencies('@preact/signals');
const jsx = dependencies('preact/jsx-runtime');
const tick = (): Promise<void> => new Promise((done) => setImmediate(done));
type Node = { type: unknown; props: Record<string, unknown> };

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
        if (id === '@preact/signals') return signals;
        if (id === 'preact/jsx-runtime') return jsx;
        if (Object.prototype.hasOwnProperty.call(imports, id)) return imports[id];
        throw new Error(`Unreviewed autosave dependency: ${id}`);
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

function hooks() {
  let cursor = 0;
  const values: unknown[] = [];
  const effects = new Map<number, { dependencies: unknown[]; cleanup?: () => void }>();
  let pending: Array<() => void> = [];
  return {
    begin() {
      cursor = 0;
    },
    flush() {
      const callbacks = pending;
      pending = [];
      callbacks.forEach((run) => run());
    },
    dispose() {
      effects.forEach((effect) => effect.cleanup?.());
      effects.clear();
    },
    useState<T>(initial: T | (() => T)): [T, (next: T | ((current: T) => T)) => void] {
      const index = cursor++;
      if (!(index in values)) values[index] = typeof initial === 'function' ? (initial as () => T)() : initial;
      return [
        values[index] as T,
        (next) => {
          values[index] = typeof next === 'function' ? (next as (current: T) => T)(values[index] as T) : next;
        },
      ];
    },
    useRef<T>(initial: T) {
      const index = cursor++;
      values[index] ??= { current: initial };
      return values[index] as { current: T };
    },
    useEffect(run: () => void | (() => void), dependencies: unknown[]) {
      const index = cursor++;
      const previous = effects.get(index);
      if (
        previous &&
        dependencies.length === previous.dependencies.length &&
        dependencies.every((value, offset) => Object.is(value, previous.dependencies[offset]))
      )
        return;
      pending.push(() => {
        previous?.cleanup?.();
        effects.set(index, { dependencies, cleanup: run() || undefined });
      });
    },
  };
}

function config(): Config {
  return {
    revision: 4,
    account_selection_revision: 2,
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
  };
}

const instance: EnrichedInstance = {
  id: '94d4ec3a-4a90-4d70-9e97-a5774f1c0a8a',
  name: 'Autosave fixture',
  version_id: '1.20.1',
  created_at: '2026-09-08T12:00:00Z',
  version_display: {
    loader_key: 'vanilla',
    loader_label: 'Vanilla',
    minecraft_label: '1.20.1',
    loader_version_label: '',
    loader_detail_label: '',
    summary_label: 'Minecraft 1.20.1',
    supports_mods: false,
  },
  launchable: true,
  launch_action: { state_id: 'ready', label: 'Launch', tone: 'ok', launchable: true, primary_action: 'launch' },
  saves_count: 0,
  mods_count: 0,
  resource_count: 0,
  shader_count: 0,
};

function controls(value: unknown): Node[] {
  if (Array.isArray(value)) return value.flatMap(controls);
  if (!value || typeof value !== 'object' || !('props' in value)) return [];
  const node = value as Node;
  return [node, ...controls(node.props.children), ...controls(node.props.control), ...controls(node.props.aside)];
}

function harness() {
  const viewHooks = hooks();
  const configState = signals.signal(config());
  const instances = signals.signal([structuredClone(instance)]);
  let storedConfig = config();
  let storedInstance = structuredClone(instance);
  const writes: Array<{ path: string; patch: Record<string, unknown> }> = [];
  const notices: string[] = [];
  const completions: Array<{ requestId: string; saved: boolean }> = [];
  const timers = new Map<number, () => void>();
  let nextTimer = 0;
  let reloads = 0;
  let listener: ((event: { payload: unknown }) => void) | undefined;
  let admit: (path: string) => void = () => {};
  let respond: (path: string, value: unknown) => Promise<unknown> = async (_path, value) => value;
  const api = {
    async api(method: string, path: string, body?: Record<string, unknown>): Promise<unknown> {
      if (method === 'GET') {
        if (path === '/config/interface-preferences')
          return {
            revision: 0,
            value: { version: 1, preferences: preferences.defaultLocalPreferences(), route: null },
          };
        if (path === '/config') return structuredClone(storedConfig);
        if (path === `/instances/${instance.id}`) return structuredClone(storedInstance);
      }
      assert.equal(method, 'PUT');
      assert.ok(body);
      writes.push({ path, patch: structuredClone(body) });
      admit(path);
      if (path === '/config') {
        const { expected_revision, expected_account_selection_revision: _selection, ...patch } = body;
        assert.equal(expected_revision, storedConfig.revision);
        storedConfig = { ...storedConfig, ...patch, revision: storedConfig.revision + 1 };
        return respond(path, structuredClone(storedConfig));
      }
      assert.equal(path, `/instances/${instance.id}`);
      storedInstance = { ...storedInstance, ...body };
      return respond(path, structuredClone(storedInstance));
    },
    isApiError: (error: unknown) => error instanceof Error && 'status' in error,
  };
  const actions = source<typeof import('../../src/actions')>('actions.ts', {
    './store': { config: configState, instances },
    './launch-response-adapters': {},
  });
  const autosave = source<typeof import('../../src/hooks/use-autosave')>('hooks/use-autosave.ts', {
    'preact/hooks': viewHooks,
    '../actions': actions,
    '../api': api,
    '../dto-core': dto,
    '../store': { config: configState },
    '../toast': { toast: (message: string) => notices.push(message) },
    '../utils': { errMessage: (error: Error) => error.message },
  });
  const music = source<typeof import('../../src/music')>(
    'music.ts',
    {
      './api': { apiResourceUrl: (path: string) => path },
      './hooks/use-autosave': autosave,
      './store': { config: configState },
      './toast': { toast: (message: string) => notices.push(message) },
    },
    {
      setTimeout(run: () => void) {
        const id = ++nextTimer;
        timers.set(id, run);
        return id;
      },
      clearTimeout(id: number) {
        timers.delete(id);
      },
    },
  );
  const native = source<typeof import('../../src/native')>(
    'native.ts',
    { './dto-contract': contract },
    {
      window: {
        __TAURI__: {
          core: {
            async invoke(command: string, args?: unknown) {
              if (command === 'pending_interface_preferences') return null;
              assert.equal(command, 'complete_interface_preferences');
              completions.push(structuredClone(args) as { requestId: string; saved: boolean });
            },
          },
          event: {
            async listen(_name: string, callback: typeof listener) {
              listener = callback;
              return () => {};
            },
          },
        },
      },
    },
  );
  const owner = source<typeof import('../../src/preferences/persistence')>(
    'preferences/persistence.ts',
    {
      '../api': api,
      '../dto-contract': contract,
      '../native': native,
      '../hooks/use-autosave': autosave,
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
  const primitive = (...names: string[]) => Object.fromEntries(names.map((name) => [name, name]));
  const common = {
    'preact/hooks': viewHooks,
    ui: primitive('OverrideChip', 'SettingRow', 'SettingsSection'),
    presets: {
      useJvmPresets: () => ({ options: [], selectable: [] }),
      normalizeJvmPreset: () => '',
      jvmPresetSelectLabel: () => '',
    },
    memory: { MemoryField: 'MemoryField', recommendedHeapRange: () => [1, 4] },
    format: {
      fmtMem: (value: number) => String(value),
      memoryGb: (value: number, fallback: number) => (value || fallback) / 1024,
    },
    store: { config: configState, systemInfo: signals.signal(null) },
  };
  const launching = source<typeof import('../../src/views/settings/LaunchingSection')>(
    'views/settings/LaunchingSection.tsx',
    {
      'preact/hooks': viewHooks,
      '../../actions': actions,
      '../../ui/Atoms': primitive('Toggle'),
      '../../ui/Select': primitive('SelectField'),
      '../../ui/SettingsSheet': common.ui,
      '../../ui/MemoryField': common.memory,
      '../../ui/WindowField': primitive('WindowField'),
      '../../ui/RuntimeFields': primitive('JavaPathField'),
      '../../hooks/use-jvm-presets': common.presets,
      '../../hooks/use-autosave': autosave,
      '../../store': common.store,
      '../../format': common.format,
    },
  );
  const audio = source<typeof import('../../src/views/settings/AudioSection')>('views/settings/AudioSection.tsx', {
    'preact/hooks': viewHooks,
    '../../ui/Atoms': primitive('Toggle'),
    '../../ui/Slider': primitive('Slider'),
    '../../ui/SettingsSheet': common.ui,
    '../../music': music,
    '../../sound': { Sound: {} },
    '../../state': {
      local: preferences.defaultLocalPreferences(),
      canEditPreferences: owner.canEditPreferences,
      saveLocalState() {},
    },
  });
  const settings = source<typeof import('../../src/views/instance/tabs/SettingsPane')>(
    'views/instance/tabs/SettingsPane.tsx',
    {
      'preact/hooks': viewHooks,
      '../../../ui/Icons': primitive('Icon'),
      '../../../ui/Select': primitive('SelectField'),
      '../../../ui/ChoicePills': primitive('ChoicePills'),
      '../../../ui/SettingsSheet': common.ui,
      '../../../ui/MemoryField': common.memory,
      '../../../ui/WindowField': primitive('WindowField'),
      '../../../ui/RuntimeFields': primitive('JavaPathField', 'JvmArgsInput'),
      '../../../hooks/use-autosave': autosave,
      '../../../hooks/use-jvm-presets': common.presets,
      '../../../api': api,
      '../../../store': common.store,
      '../../../actions': actions,
      '../../../format': common.format,
      '../../../dto-core': dto,
      '../../../performance-presenters': { performanceHealthNotice: () => null },
      '../performance-mode': {
        fetchPerformanceHealth: async () => null,
        globalPerformanceMode: () => 'vanilla',
        performanceModeFrom: () => '',
        performanceModeLabel: () => 'Vanilla',
      },
    },
    {
      window: {
        setTimeout(run: () => void) {
          const id = ++nextTimer;
          timers.set(id, run);
          return id;
        },
        clearTimeout(id: number) {
          timers.delete(id);
        },
      },
    },
  );
  let tree: unknown;
  return {
    owner,
    autosave,
    music: music.Music,
    writes,
    notices,
    completions,
    timers,
    config: () => storedConfig,
    instance: () => storedInstance,
    reloads: () => reloads,
    respond(next: typeof respond) {
      respond = next;
    },
    admit(next: typeof admit) {
      admit = next;
    },
    async hydrate() {
      await owner.initializeNativePreferences(config());
    },
    render(view: 'global' | 'instance' | 'audio') {
      viewHooks.begin();
      if (view === 'global') tree = launching.LaunchingSection();
      else if (view === 'audio') tree = audio.AudioSection();
      else {
        const wrapper = settings.SettingsPane({ inst: instances.value[0] }) as unknown as Node;
        tree = (wrapper.type as (props: unknown) => unknown)(wrapper.props);
      }
      viewHooks.flush();
    },
    control<T>(name: string): T {
      const node = controls(tree).find((node) => node.type === name);
      assert.ok(node, name);
      return node.props as T;
    },
    emit(phase: NativePreferencesRequest['phase'], id = 'preferences-1') {
      assert.ok(listener);
      listener({ payload: { request_id: id, phase } });
    },
    timersRun() {
      const callbacks = [...timers.values()];
      timers.clear();
      callbacks.forEach((run) => run());
    },
    dispose: viewHooks.dispose,
  };
}

test('native flush joins a committed settings reply and the next queued real control before acknowledging', async () => {
  const h = harness();
  await h.hydrate();
  h.render('global');
  const first = deferred<unknown>();
  const second = deferred<unknown>();
  let firstResponse: unknown;
  let secondResponse: unknown;
  h.respond(async (_path, value) => {
    if (h.writes.length === 1) {
      firstResponse = value;
      return first.promise;
    }
    secondResponse = value;
    return second.promise;
  });
  h.control<{ onCommit(low: number, high: number): void }>('MemoryField').onCommit(1, 5);
  await tick();
  assert.equal(h.config().max_memory_mb, 5120);
  h.control<{ onCommit(width: number, height: number): void }>('WindowField').onCommit(1000, 600);
  h.emit('flush');
  try {
    await tick();
    assert.deepEqual(h.completions, []);
    assert.equal(h.writes.length, 1);
    first.resolve(firstResponse);
    await tick();
    assert.equal(h.writes.length, 2);
    assert.deepEqual(h.completions, []);
    second.resolve(secondResponse);
    await tick();
    assert.deepEqual(h.completions, [{ requestId: 'preferences-1', saved: true }]);
    assert.equal(h.config().window_width, 1000);
  } finally {
    first.resolve(firstResponse);
    second.resolve(secondResponse);
    await tick();
    h.dispose();
  }
});

test('native flush submits the real instance JVM draft before its debounce and joins its receipt', async () => {
  const h = harness();
  await h.hydrate();
  h.render('instance');
  await tick();
  const reply = deferred<unknown>();
  let response: unknown;
  h.respond(async (_path, value) => {
    response = value;
    return reply.promise;
  });
  h.control<{ onChange(value: string): void }>('JvmArgsInput').onChange('-Daxial.fixture=true');
  assert.equal(h.writes.length, 0);
  h.emit('flush');
  try {
    await tick();
    assert.equal(h.writes.length, 1);
    assert.equal(h.writes[0].patch.extra_jvm_args, '-Daxial.fixture=true');
    assert.deepEqual(h.completions, []);
    reply.resolve(response);
    await tick();
    assert.deepEqual(h.completions, [{ requestId: 'preferences-1', saved: true }]);
    h.timersRun();
    await tick();
    assert.equal(h.writes.length, 1);
  } finally {
    reply.resolve(response);
    h.dispose();
    await tick();
  }
});

test('native discard joins the sent config write, parks the queued control, and resumes it once on release', async () => {
  const h = harness();
  await h.hydrate();
  h.render('global');
  const reply = deferred<unknown>();
  let response: unknown;
  h.respond(async (_path, value) => {
    if (h.writes.length === 1) {
      response = value;
      return reply.promise;
    }
    return value;
  });
  h.control<{ onCommit(low: number, high: number): void }>('MemoryField').onCommit(1, 5);
  await tick();
  h.control<{ onCommit(width: number, height: number): void }>('WindowField').onCommit(1000, 600);
  h.emit('discard');
  await tick();
  assert.deepEqual(h.completions, []);
  reply.resolve(response);
  await tick();
  assert.deepEqual(h.completions, [{ requestId: 'preferences-1', saved: true }]);
  assert.equal(h.writes.length, 1);
  assert.equal(h.config().window_width, 854);
  h.emit('release');
  await tick();
  assert.equal(h.writes.length, 2);
  assert.equal(h.writes[1].patch.expected_revision, 5);
  assert.equal(h.config().window_width, 1000);
  h.emit('release');
  await tick();
  assert.equal(h.writes.length, 2);
  h.dispose();
});

test('native discard retains an unmounted JVM draft without a timer write until release', async () => {
  const h = harness();
  await h.hydrate();
  h.render('instance');
  const input = h.control<{ onChange(value: string): void }>('JvmArgsInput');
  input.onChange('-Daxial.pending=true');
  h.emit('discard');
  await tick();
  assert.deepEqual(h.completions, [{ requestId: 'preferences-1', saved: true }]);
  h.timersRun();
  input.onChange('-Daxial.too-late=true');
  h.dispose();
  await tick();
  assert.equal(h.writes.length, 0);
  h.emit('release');
  await tick();
  assert.equal(h.writes.length, 1);
  assert.equal(h.instance().extra_jvm_args, '-Daxial.pending=true');
  h.timersRun();
  h.emit('release');
  await tick();
  assert.equal(h.writes.length, 1);
});

test('native discard also parks the existing instance target queue behind its sent write', async () => {
  const h = harness();
  await h.hydrate();
  h.render('instance');
  await tick();
  const reply = deferred<unknown>();
  let response: unknown;
  h.respond(async (_path, value) => {
    if (h.writes.length === 1) {
      response = value;
      return reply.promise;
    }
    return value;
  });
  h.control<{ onCommit(low: number, high: number): void }>('MemoryField').onCommit(1, 5);
  await tick();
  h.control<{ onCommit(width: number, height: number): void }>('WindowField').onCommit(1000, 600);
  h.emit('discard');
  await tick();
  assert.deepEqual(h.completions, []);
  reply.resolve(response);
  await tick();
  assert.equal(h.writes.length, 1);
  assert.deepEqual(h.completions, [{ requestId: 'preferences-1', saved: true }]);
  h.emit('release');
  await tick();
  assert.equal(h.writes.length, 2);
  assert.equal(h.instance().window_width, 1000);
  h.dispose();
});

test('native flush reports an outstanding failed save but does not poison a later released attempt', async () => {
  const h = harness();
  await h.hydrate();
  h.render('global');
  const reply = deferred<unknown>();
  h.respond(async () => reply.promise);
  h.control<{ onCommit(low: number, high: number): void }>('MemoryField').onCommit(1, 5);
  await tick();
  h.emit('flush');
  reply.reject(new Error('Fixture response lost'));
  await tick();
  assert.deepEqual(h.completions, [{ requestId: 'preferences-1', saved: false }]);
  assert.ok(h.notices.some((notice) => notice.includes('Could not save')));
  assert.equal(h.writes.length, 1);
  await assert.rejects(h.autosave.saveConfigPatch({ window_width: 1600 }), /paused/);
  assert.equal(h.writes.length, 1);
  h.emit('release');
  h.respond(async (_path, value) => value);
  h.control<{ onCommit(width: number, height: number): void }>('WindowField').onCommit(1000, 600);
  h.emit('flush', 'preferences-2');
  await tick();
  assert.deepEqual(h.completions, [
    { requestId: 'preferences-1', saved: false },
    { requestId: 'preferences-2', saved: true },
  ]);
  assert.equal(h.writes.length, 2);
  assert.equal(h.writes[1].patch.expected_revision, 5);
  h.dispose();
});

test('new flush supersedes discard without letting a stale release unseal late edits', async () => {
  const h = harness();
  await h.hydrate();
  h.render('instance');
  h.control<{ onChange(value: string): void }>('JvmArgsInput').onChange('-Daxial.pending=true');
  h.emit('discard');
  await tick();
  const reply = deferred<unknown>();
  let response: unknown;
  h.respond(async (_path, value) => {
    response = value;
    return reply.promise;
  });
  h.emit('flush', 'preferences-2');
  h.emit('release', 'preferences-1');
  await tick();
  assert.equal(h.writes.length, 1);
  assert.deepEqual(h.completions, [{ requestId: 'preferences-1', saved: true }]);
  h.control<{ onChange(value: string): void }>('JvmArgsInput').onChange('-Daxial.too-late=true');
  h.timersRun();
  reply.resolve(response);
  await tick();
  assert.deepEqual(h.completions, [
    { requestId: 'preferences-1', saved: true },
    { requestId: 'preferences-2', saved: true },
  ]);
  assert.equal(h.writes.length, 1);
  assert.equal(h.instance().extra_jvm_args, '-Daxial.pending=true');
  h.dispose();
});

test('interface preference refusal cannot acknowledge flush while a config response remains outstanding', async () => {
  const h = harness();
  await h.hydrate();
  h.render('global');
  const reply = deferred<unknown>();
  let response: unknown;
  h.respond(async (_path, value) => {
    response = value;
    return reply.promise;
  });
  h.admit((path) => {
    if (path === '/config/interface-preferences') throw Object.assign(new Error('Fixture refusal'), { status: 409 });
  });
  h.control<{ onCommit(low: number, high: number): void }>('MemoryField').onCommit(1, 5);
  await tick();
  h.owner.saveNativeRoute({ name: 'settings' });
  h.emit('flush');
  await tick();
  assert.deepEqual(h.completions, []);
  reply.resolve(response);
  await tick();
  assert.deepEqual(h.completions, [{ requestId: 'preferences-1', saved: false }]);
  assert.equal(h.writes.length, 2);
  h.dispose();
});

test('a refused JVM draft keeps the existing revert and error behavior without a silent retry', async () => {
  const h = harness();
  await h.hydrate();
  h.render('instance');
  await tick();
  h.admit(() => {
    throw Object.assign(new Error('Fixture refusal'), { status: 409 });
  });
  h.control<{ onChange(value: string): void }>('JvmArgsInput').onChange('-Daxial.refused=true');
  h.emit('flush');
  await tick();
  assert.deepEqual(h.completions, [{ requestId: 'preferences-1', saved: false }]);
  assert.equal(h.instance().extra_jvm_args, undefined);
  h.render('instance');
  assert.equal(h.control<{ value: string }>('JvmArgsInput').value, '');
  assert.ok(h.notices.some((notice) => notice.includes('Could not save JVM arguments: Fixture refusal')));
  h.emit('release');
  h.timersRun();
  await tick();
  assert.equal(h.writes.length, 1);
  h.dispose();
});

test('native flush submits the real audio slider draft and joins the music settings receipt', async () => {
  const h = harness();
  await h.hydrate();
  h.music.applyConfig({ music_enabled: true, music_volume: 20, music_track: 0 });
  h.render('audio');
  const reply = deferred<unknown>();
  let response: unknown;
  h.respond(async (_path, value) => {
    response = value;
    return reply.promise;
  });
  h.control<{ onChange(value: number): void }>('Slider').onChange(70);
  assert.equal(h.writes.length, 0);
  h.emit('flush');
  try {
    await tick();
    assert.equal(h.writes.length, 1);
    assert.equal(h.writes[0].patch.music_volume, 70);
    assert.deepEqual(h.completions, []);
    reply.resolve(response);
    await tick();
    assert.deepEqual(h.completions, [{ requestId: 'preferences-1', saved: true }]);
    h.timersRun();
    await tick();
    assert.equal(h.writes.length, 1);
  } finally {
    reply.resolve(response);
    h.dispose();
  }
});

test('native discard parks the music slider timer and late edits until release', async () => {
  const h = harness();
  await h.hydrate();
  h.music.applyConfig({ music_enabled: true, music_volume: 20, music_track: 0 });
  h.render('audio');
  const slider = h.control<{ onChange(value: number): void }>('Slider');
  slider.onChange(70);
  h.emit('discard');
  await tick();
  h.timersRun();
  await tick();
  assert.equal(h.writes.length, 0);
  assert.equal(h.music.volume, 70);
  slider.onChange(90);
  assert.equal(h.music.volume, 70);
  h.emit('release');
  await tick();
  assert.equal(h.writes.length, 1);
  assert.equal(h.config().music_volume, 70);
  h.timersRun();
  await tick();
  assert.equal(h.writes.length, 1);
  h.dispose();
});

test('an imported music snapshot supersedes the parked slider draft before native release', async () => {
  const h = harness();
  await h.hydrate();
  h.music.applyConfig({ music_enabled: true, music_volume: 20, music_track: 0 });
  h.render('audio');
  h.control<{ onChange(value: number): void }>('Slider').onChange(70);
  h.emit('discard');
  h.timersRun();
  h.music.applyConfig({ music_enabled: false, music_volume: 35, music_track: 1 }, true);
  h.emit('release');
  h.timersRun();
  await tick();
  assert.equal(h.music.volume, 35);
  assert.equal(h.music.enabled, false);
  assert.equal(h.writes.length, 0);
  h.dispose();
});

test('Reload joins the held config response and queued real control before replacing the renderer', async () => {
  const h = harness();
  await h.hydrate();
  h.render('global');
  const first = deferred<unknown>();
  const second = deferred<unknown>();
  let firstResponse: unknown;
  let secondResponse: unknown;
  h.respond(async (_path, value) => {
    if (h.writes.length === 1) {
      firstResponse = value;
      return first.promise;
    }
    secondResponse = value;
    return second.promise;
  });
  h.control<{ onCommit(low: number, high: number): void }>('MemoryField').onCommit(1, 5);
  await tick();
  h.control<{ onCommit(width: number, height: number): void }>('WindowField').onCommit(1000, 600);
  const reloading = h.owner.reloadApplication();
  assert.equal(h.owner.reloadApplication(), reloading);
  try {
    await tick();
    assert.equal(h.reloads(), 0);
    first.resolve(firstResponse);
    await tick();
    assert.equal(h.writes.length, 2);
    assert.equal(h.reloads(), 0);
    second.resolve(secondResponse);
    assert.equal(await reloading, true);
    assert.equal(h.reloads(), 1);
    assert.equal(h.config().window_width, 1000);
    await assert.rejects(h.autosave.saveConfigPatch({ window_width: 1600 }), /paused/);
  } finally {
    first.resolve(firstResponse);
    second.resolve(secondResponse);
    await tick();
    h.dispose();
  }
});

for (const view of ['instance', 'audio'] as const) {
  test(`Reload flushes the real ${view} draft and waits for its receipt`, async () => {
    const h = harness();
    await h.hydrate();
    if (view === 'audio') h.music.applyConfig({ music_enabled: true, music_volume: 20, music_track: 0 });
    h.render(view);
    await tick();
    const reply = deferred<unknown>();
    let response: unknown;
    h.respond(async (_path, value) => {
      response = value;
      return reply.promise;
    });
    if (view === 'instance')
      h.control<{ onChange(value: string): void }>('JvmArgsInput').onChange('-Daxial.reload=true');
    else h.control<{ onChange(value: number): void }>('Slider').onChange(70);
    const reloading = h.owner.reloadApplication();
    try {
      await tick();
      assert.equal(h.writes.length, 1);
      assert.equal(h.reloads(), 0);
      assert.equal(
        view === 'instance' ? h.writes[0].patch.extra_jvm_args : h.writes[0].patch.music_volume,
        view === 'instance' ? '-Daxial.reload=true' : 70,
      );
      reply.resolve(response);
      assert.equal(await reloading, true);
      assert.equal(h.reloads(), 1);
      h.timersRun();
      await tick();
      assert.equal(h.writes.length, 1);
    } finally {
      reply.resolve(response);
      h.dispose();
      await tick();
    }
  });
}

test('a refused settings save prevents Reload and releases the real controls for an explicit retry', async () => {
  const h = harness();
  await h.hydrate();
  h.render('global');
  h.admit(() => {
    throw Object.assign(new Error('Fixture refusal'), { status: 409 });
  });
  h.control<{ onCommit(low: number, high: number): void }>('MemoryField').onCommit(1, 5);
  assert.equal(await h.owner.reloadApplication(), false);
  assert.equal(h.reloads(), 0);
  assert.equal(h.writes.length, 1);
  assert.equal(h.config().max_memory_mb, 4096);
  assert.equal(h.owner.canEditPreferences(), true);
  h.admit(() => {});
  h.control<{ onCommit(width: number, height: number): void }>('WindowField').onCommit(1000, 600);
  assert.equal(await h.owner.reloadApplication(), true);
  assert.equal(h.reloads(), 1);
  assert.equal(h.writes.length, 2);
  assert.equal(h.config().window_width, 1000);
  h.dispose();
});

test('a native request and release fence an older Reload even after the native owner becomes idle again', async () => {
  const h = harness();
  await h.hydrate();
  h.render('global');
  const reply = deferred<unknown>();
  let response: unknown;
  h.respond(async (_path, value) => {
    response = value;
    return reply.promise;
  });
  h.control<{ onCommit(low: number, high: number): void }>('MemoryField').onCommit(1, 5);
  await tick();
  const reloading = h.owner.reloadApplication();
  h.emit('discard');
  h.emit('release');
  reply.resolve(response);
  assert.equal(await reloading, false);
  assert.equal(h.reloads(), 0);
  assert.equal(h.owner.canEditPreferences(), true);
  assert.equal(await h.owner.reloadApplication(), true);
  assert.equal(h.reloads(), 1);
  h.dispose();
});
