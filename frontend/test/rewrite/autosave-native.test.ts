import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import * as contract from '../../src/dto-contract';
import * as dto from '../../src/dto-core';
import * as installDto from '../../src/dto-install';
import * as installItems from '../../src/install-item';
import * as downloadViews from '../../src/machines/download-view-models';
import * as preferences from '../../src/preferences/local';
import type { Config } from '../../src/types-settings';
import type { EnrichedInstance } from '../../src/types-instance';
import type { NativePreferencesRequest } from '../../src/native';
import type { InstallQueueStateResponse } from '../../src/types-install';
import type { LaunchSession } from '../../src/types-launch';
import type { LaunchState } from '../../src/store';

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
    useMemo<T>(calculate: () => T, dependencies: unknown[]): T {
      const index = cursor++;
      const previous = values[index] as { dependencies: unknown[]; value: T } | undefined;
      if (!previous || dependencies.length !== previous.dependencies.length ||
        dependencies.some((value, offset) => !Object.is(value, previous.dependencies[offset]))) {
        values[index] = { dependencies, value: calculate() };
      }
      return (values[index] as { value: T }).value;
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
  revision: 6,
  name: 'Autosave fixture',
  version_id: '1.20.1',
  created_at: '2026-09-08T12:00:00Z',
  java_selection: { kind: 'inherited' },
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

function queue(revision = 1): InstallQueueStateResponse {
  return {
    queue_epoch: 'current-process', revision, registry_revision: 2,
    active: null, items: [], latest_failure: null,
    view_model: { state_id: 'empty', status_label: 'Idle', title: 'Downloads', summary: '', queued_count: 0,
      queued_count_label: '0', queued_item_label: 'Queued', section_title: 'Queue', empty_title: 'Empty', empty_summary: '' },
  };
}

function harness() {
  const viewHooks = hooks();
  const javaHooks = hooks();
  const configState = signals.signal(config());
  const instances = signals.signal([structuredClone(instance)]);
  const store = {
    config: configState, instances, versions: signals.signal([]), lastInstanceId: signals.signal<string | null>(null),
    launchSessions: signals.signal<Record<string, LaunchSession>>({}),
    launchState: signals.signal<LaunchState>({ status: 'idle' }),
  };
  let storedConfig = config();
  let storedInstance = structuredClone(instance);
  const writes: Array<{ path: string; patch: Record<string, unknown> }> = [];
  const reads: string[] = [];
  const notices: string[] = [];
  const completions: Array<{ requestId: string; saved: boolean }> = [];
  const timers = new Map<number, () => void>();
  let nextTimer = 0;
  let reloads = 0;
  let healthReads = 0;
  let listener: ((event: { payload: unknown }) => void) | undefined;
  let admit: (path: string) => void = () => {};
  let respond: (path: string, value: unknown) => Promise<unknown> = async (_path, value) => value;
  let readInstance: (value: EnrichedInstance) => Promise<unknown> = async (value) => value;
  let currentQueue = queue();
  let queueSubscription: ((snapshot: InstallQueueStateResponse) => void) | undefined;
  const clock = {
    setTimeout(run: () => void) {
      const id = ++nextTimer;
      timers.set(id, run);
      return id;
    },
    clearTimeout(id: number) { timers.delete(id); },
  };
  const api = {
    async api(method: string, path: string, body?: Record<string, unknown>): Promise<unknown> {
      if (method === 'GET') {
        reads.push(path);
        if (path === '/java') return { runtimes: [] };
        if (path === '/install/queue') return currentQueue;
        if (path === '/versions') return { versions: [] };
        if (path === '/instances') return { instances: [structuredClone(storedInstance)], last_instance_id: instance.id };
        if (path === '/config/interface-preferences')
          return {
            revision: 0,
            value: { version: 1, preferences: preferences.defaultLocalPreferences(), route: null },
          };
        if (path === '/config') return structuredClone(storedConfig);
        if (path === `/instances/${instance.id}`) return readInstance(structuredClone(storedInstance));
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
      const { expected_revision, ...patch } = body;
      if (expected_revision !== undefined && expected_revision !== storedInstance.revision) {
        throw Object.assign(new Error('The instance changed. Refresh and try again.'), { name: 'ApiError', status: 409 });
      }
      storedInstance = { ...storedInstance, ...patch, revision: storedInstance.revision + 1 };
      return respond(path, structuredClone(storedInstance));
    },
    isApiError: (error: unknown) => error instanceof Error && 'status' in error,
  };
  const actions = source<typeof import('../../src/actions')>('actions.ts', {
    './store': store,
    './launch-response-adapters': {},
  });
  const readiness = source<typeof import('../../src/instance-readiness')>('instance-readiness.ts', {
    './api': api, './dto-core': dto, './store': store,
    './utils': { showError: (message: string) => notices.push(message) },
  }, { window: clock });
  const downloads = source<typeof import('../../src/machines/downloads')>('machines/downloads.ts', {
    '../api': api,
    '../utils': { errMessage: String, showError: (message: string) => notices.push(message) },
    '../toast': { toast: (message: string) => notices.push(message) },
    '../loaders/api': { connectInstallQueueSSE(next: typeof queueSubscription) {
      queueSubscription = next;
      return () => { queueSubscription = undefined; };
    } },
    '../store': store, '../content-activity': { markContentChanged() {} },
    '../dto-install': installDto, '../dto-core': dto, '../install-item': installItems,
    './download-view-models': downloadViews,
  }, clock);
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
  const runtimeFields = source<typeof import('../../src/ui/RuntimeFields')>('ui/RuntimeFields.tsx', {
    'preact/hooks': javaHooks,
    './Select': primitive('SelectField'),
    './Icons': primitive('Icon'),
    '../api': api,
    '../dto-contract': contract,
  });
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
    store: { config: configState, instances, systemInfo: signals.signal(null) },
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
      '../../../ui/RuntimeFields': { JavaPathField: runtimeFields.JavaPathField, JvmArgsInput: 'JvmArgsInput' },
      '../../../hooks/use-autosave': autosave,
      '../../../hooks/use-jvm-presets': common.presets,
      '../../../api': api,
      '../../../instance-readiness': readiness,
      '../../../store': common.store,
      '../../../actions': actions,
      '../../../format': common.format,
      '../../../dto-core': dto,
      '../../../performance-presenters': { performanceHealthNotice: () => null },
      '../performance-mode': {
        fetchPerformanceHealth: async () => {
          healthReads++;
          return null;
        },
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
    downloads,
    readiness,
    store,
    music: music.Music,
    writes,
    reads,
    notices,
    completions,
    timers,
    config: () => storedConfig,
    instance: () => storedInstance,
    serverInstance(value: EnrichedInstance) { storedInstance = structuredClone(value); },
    reloads: () => reloads,
    healthReads: () => healthReads,
    respond(next: typeof respond) {
      respond = next;
    },
    readInstance(next: typeof readInstance) { readInstance = next; },
    emitQueue(snapshot: InstallQueueStateResponse) {
      currentQueue = snapshot;
      assert.ok(queueSubscription);
      queueSubscription(snapshot);
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
    renderJava(): unknown {
      const node = controls(tree).find((entry) => entry.type === runtimeFields.JavaPathField);
      assert.ok(node, 'SettingsPane must render the real JavaPathField');
      javaHooks.begin();
      const rendered = runtimeFields.JavaPathField(node.props as Parameters<typeof runtimeFields.JavaPathField>[0]);
      javaHooks.flush();
      return rendered;
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
    dispose() {
      viewHooks.dispose();
      javaHooks.dispose();
      downloads.disconnectInstallQueue();
    },
  };
}

test('cold instance Java settings retain redacted selection and explicit edits', async (context) => {
  const component = 'java-runtime-delta';
  for (const scenario of ['untouched custom', 'clear custom', 'replace custom', 'inherit global custom', 'component'] as const) {
    await context.test(scenario, async () => {
      const h = harness();
      const inherited = scenario === 'inherit global custom';
      const selection = inherited ? { kind: 'inherited' } : scenario === 'component'
        ? { kind: 'component', component } : { kind: 'custom' };
      const detail = { ...instance, java_path: '', extra_jvm_args: '', java_selection: selection };
      h.readInstance(async () => JSON.parse(JSON.stringify(detail)));
      h.store.config.value = { ...config(), java_path_override: inherited ? '/fixture/global-java' : '' };
      const clears = scenario === 'clear custom' || scenario === 'component';
      h.respond(async () => ({
        ...detail, revision: 7, java_selection: clears ? { kind: 'inherited' } : { kind: 'custom' },
      }));
      const render = (): unknown => {
        h.render('instance');
        return h.renderJava();
      };
      const select = (tree: unknown) => {
        const node = controls(tree).find((entry) => entry.type === 'SelectField' && entry.props.ariaLabel === 'Java runtime');
        assert.ok(node, 'the real JavaPathField must render its selector');
        return node.props as {
          value: string;
          options: Array<{ value: string; label: string }>;
          onChange(value: string): void;
        };
      };
      const input = (tree: unknown) => {
        const node = controls(tree).find((entry) => entry.type === 'input' && entry.props['aria-label'] === 'Custom Java path');
        assert.ok(node, 'the real JavaPathField must render its custom editor');
        return node.props as {
          value: string;
          onInput(event: { currentTarget: { value: string } }): void;
          onBlur(): void;
        };
      };
      try {
        h.render('instance');
        await tick();
        render();
        await tick();
        let tree = render();
        const chooser = select(tree);
        assert.equal(h.reads.filter((path) => path === `/instances/${instance.id}`).length, 1);
        assert.equal(h.reads.filter((path) => path === '/java').length, 1);
        assert.equal(h.store.instances.value[0].java_path, '');
        assert.equal(h.store.instances.value[0].extra_jvm_args, '');
        assert.equal(chooser.options.find((option) => option.value === chooser.value)?.label,
          inherited ? 'Inherit global Java' : scenario === 'component' ? component : 'Custom path…');
        assert.equal(chooser.options.find((option) => option.value === '')?.label, 'Inherit global Java');
        assert.equal(chooser.value, inherited ? '' : scenario === 'component' ? component : '__custom__');
        assert.equal(h.writes.length, 0, 'cold detail hydration must not write a redacted value back');
        if (inherited) {
          chooser.onChange('__custom__');
          tree = render();
          assert.equal(input(tree).value, '');
          input(tree).onBlur();
          await tick();
          assert.equal(h.store.config.value.java_path_override, '/fixture/global-java');
          assert.equal(h.writes.length, 0, 'opening an empty editor must retain global inheritance');
        } else if (clears) {
          chooser.onChange('');
          await tick();
          assert.deepEqual(h.writes, [{ path: `/instances/${instance.id}`, patch: { expected_revision: 6, java_path: '' } }]);
          tree = render();
          assert.equal(select(tree).value, '', 'the acknowledged clear must display inherited selection');
        } else {
          assert.equal(input(tree).value, '', 'saved private paths must not populate the editor');
          input(tree).onBlur();
          await tick();
          assert.equal(h.writes.length, 0, 'an untouched redacted editor must not clear the saved override');
          if (scenario === 'replace custom') {
            input(tree).onInput({ currentTarget: { value: '/fixture/replacement-java' } });
            tree = render();
            input(tree).onBlur();
            await tick();
            assert.deepEqual(h.writes, [{
              path: `/instances/${instance.id}`, patch: { expected_revision: 6, java_path: '/fixture/replacement-java' },
            }]);
            tree = render();
            assert.equal(select(tree).options.find((option) => option.value === select(tree).value)?.label, 'Custom path…');
            assert.equal(h.store.instances.value[0].java_path, '');
            assert.equal(h.store.instances.value[0].extra_jvm_args, '');
          }
        }
        assert.equal(h.timers.size, 0);
      } finally {
        h.dispose();
        await tick();
      }
    });
  }
});

test('a stale Runtime Reset carries its captured revision and preserves a newer component', async () => {
  const h = harness();
  const captured = {
    ...instance, revision: 6, java_path: '', extra_jvm_args: '', java_selection: { kind: 'custom' },
  };
  const newer = { ...captured, revision: 7, java_selection: { kind: 'component', component: 'jre-legacy' } };
  const cleared = { ...captured, revision: 8, java_selection: { kind: 'inherited' } };
  let serverDetail: unknown = captured;
  h.readInstance(async () => structuredClone(serverDetail));
  h.admit(() => {
    // The real endpoint refuses revision 6 against 7, but accepts an unversioned patch.
    if (h.writes[h.writes.length - 1]?.patch.expected_revision === 6) {
      throw Object.assign(new Error('The instance changed. Refresh and try again.'), { name: 'ApiError', status: 409 });
    }
  });
  h.respond(async () => {
    serverDetail = cleared;
    return structuredClone(cleared);
  });
  let refreshed: Record<string, unknown> | undefined;
  try {
    h.render('instance');
    await tick();
    h.render('instance');
    assert.deepEqual(h.store.instances.value[0].java_selection, { kind: 'custom' });
    const reset = h.control<{ onReset(): void }>('OverrideChip');
    serverDetail = newer;
    reset.onReset();
    await tick();
    await h.readiness.refreshInstanceReadiness(instance.id, { retry: false });
    refreshed = JSON.parse(JSON.stringify(h.store.instances.value[0])) as Record<string, unknown>;
  } finally {
    h.dispose();
    await tick();
  }
  assert.deepEqual(h.writes, [{
    path: `/instances/${instance.id}`,
    patch: { expected_revision: 6, jvm_preset: '', java_path: '' },
  }], 'Reset must bind the rendered revision, not omit it or refresh its evidence before sending');
  assert.ok(h.notices.some((notice) => notice.includes('Could not save runtime: The instance changed.')));
  assert.ok(!h.notices.includes('Saved'));
  assert.deepEqual(refreshed?.java_selection, { kind: 'component', component: 'jre-legacy' });
  assert.equal(refreshed?.revision, 7);
  assert.equal(h.timers.size, 0);
});

test('native flush joins two rapid instance controls through their own acknowledged revisions', async () => {
  const h = harness();
  const first = deferred<unknown>();
  let firstResponse: unknown;
  await h.hydrate();
  try {
    h.render('instance');
    await tick();
    h.render('instance');
    h.respond(async (_path, value) => {
      if (h.writes.length !== 1) return value;
      firstResponse = value;
      return first.promise;
    });
    h.control<{ onCommit(low: number, high: number): void }>('MemoryField').onCommit(1, 5);
    h.control<{ onCommit(width: number, height: number): void }>('WindowField').onCommit(1000, 600);
    h.emit('flush');
    await tick();
    assert.equal(h.instance().revision, 7);
    assert.equal(h.writes.length, 1);
    assert.deepEqual(h.completions, []);
    first.resolve(firstResponse);
    await tick();
  } finally {
    first.resolve(firstResponse);
    await tick();
    h.dispose();
  }
  assert.deepEqual(h.writes.map((write) => write.patch.expected_revision), [6, 7]);
  assert.deepEqual(h.completions, [{ requestId: 'preferences-1', saved: true }]);
  assert.equal(h.store.instances.value[0].revision, 8);
  assert.equal(h.store.instances.value[0].min_memory_mb, 1024);
  assert.equal(h.store.instances.value[0].max_memory_mb, 5120);
  assert.equal(h.store.instances.value[0].window_width, 1000);
  assert.equal(h.store.instances.value[0].window_height, 600);
  assert.equal(h.instance().revision, 8);
  assert.equal(h.timers.size, 0);
});

test('launch metadata saves leave managed-file health inspection to mode changes', async () => {
  const h = harness();
  try {
    h.render('instance');
    await tick();
    h.render('instance');
    assert.equal(h.healthReads(), 1);
    h.control<{ onCommit(low: number, high: number): void }>('MemoryField').onCommit(1, 2);
    await tick();
    h.render('instance');
    await tick();
    assert.equal(h.instance().max_memory_mb, 2048);
    assert.equal(h.healthReads(), 1);
    assert.equal(h.writes.length, 1);
    h.control<{ onCommit(width: number, height: number): void }>('WindowField').onCommit(1000, 600);
    await tick();
    h.render('instance');
    h.control<{ onChange(value: string): void }>('JvmArgsInput').onChange('-Daxial.fixture=true');
    h.timersRun();
    await tick();
    h.render('instance');
    await tick();
    assert.equal(h.healthReads(), 1);
    assert.equal(h.instance().window_width, 1000);
    assert.equal(h.instance().extra_jvm_args, '-Daxial.fixture=true');
    h.control<{ onChange(value: string): void }>('ChoicePills').onChange('custom');
    await tick();
    h.render('instance');
    await tick();
    assert.equal(h.instance().performance_mode, 'custom');
    assert.equal(h.healthReads(), 2);
    assert.equal(h.writes.length, 4);
  } finally {
    h.dispose();
    await tick();
  }
});

test('a refused older memory control restores the latest acknowledged heap, not its captured value', async () => {
  const h = harness();
  h.serverInstance({ ...instance, min_memory_mb: 1024, max_memory_mb: 8192 });
  try {
    h.render('instance');
    await tick();
    h.render('instance');
    h.render('instance');
    const memory = h.control<{ onChange(low: number, high: number): void; onCommit(low: number, high: number): void }>(
      'MemoryField',
    );
    memory.onChange(1, 1);
    memory.onCommit(1, 1);
    await tick();
    h.render('instance');
    h.render('instance');
    assert.equal(h.control<{ maxGb: number }>('MemoryField').maxGb, 1);
    memory.onChange(1, 2);
    memory.onCommit(1, 2);
    await tick();
    h.render('instance');
    assert.equal(h.instance().max_memory_mb, 1024);
    assert.equal(h.control<{ maxGb: number }>('MemoryField').maxGb, 1);
    assert.equal(h.writes.length, 2, 'a refused edit must not replay');
    assert.ok(h.notices.some((notice) => notice.includes('Could not save memory: The instance changed.')));
  } finally {
    h.dispose();
    await tick();
  }
});

test('an older rendered control cannot borrow an independently admitted revision chain', async () => {
  const h = harness();
  const first = deferred<unknown>();
  let firstResponse: unknown;
  await h.hydrate();
  try {
    h.render('instance');
    await tick();
    h.render('instance');
    const oldMemory = h.control<{ onCommit(low: number, high: number): void }>('MemoryField');
    h.serverInstance({ ...instance, revision: 7, window_width: 900 });
    await h.readiness.refreshInstanceReadiness(instance.id, { retry: false });
    h.render('instance');
    h.respond(async (_path, value) => {
      if (h.writes.length !== 1) return value;
      firstResponse = value;
      return first.promise;
    });
    h.control<{ onCommit(width: number, height: number): void }>('WindowField').onCommit(1000, 600);
    oldMemory.onCommit(1, 5);
    h.emit('flush');
    await tick();
    assert.equal(h.instance().revision, 8);
    assert.equal(h.writes.length, 1);
    assert.deepEqual(h.completions, []);
    first.resolve(firstResponse);
    await tick();
  } finally {
    first.resolve(firstResponse);
    await tick();
    h.dispose();
  }
  assert.deepEqual(h.writes.map((write) => write.patch.expected_revision), [7, 6]);
  assert.deepEqual(h.completions, [{ requestId: 'preferences-1', saved: false }]);
  assert.ok(h.notices.some((notice) => notice.includes('Could not save memory: The instance changed.')));
  assert.equal(h.instance().revision, 8);
  assert.equal(h.instance().window_width, 1000);
  assert.equal(h.instance().max_memory_mb, undefined);
  assert.equal(h.store.instances.value[0].revision, 8);
  assert.equal(h.timers.size, 0);
});

test('pending instance controls cannot borrow external changes or untrusted acknowledgements', async (context) => {
  for (const outcome of ['external change', 'refusal', 'lost reply', 'missing revision', 'fractional revision', 'wrong instance', 'jumped revision'] as const) {
    await context.test(outcome, async () => {
      const h = harness();
      const first = deferred<unknown>();
      let firstResponse: unknown;
      await h.hydrate();
      try {
        h.render('instance');
        await tick();
        h.render('instance');
        if (outcome === 'refusal') h.admit(() => {
          if (h.writes.length === 1) {
            throw Object.assign(new Error('The instance changed. Refresh and try again.'), { name: 'ApiError', status: 409 });
          }
        });
        h.respond(async (_path, value) => {
          if (h.writes.length !== 1) return value;
          firstResponse = value;
          return first.promise;
        });
        h.control<{ onCommit(low: number, high: number): void }>('MemoryField').onCommit(1, 5);
        h.control<{ onCommit(width: number, height: number): void }>('WindowField').onCommit(1000, 600);
        h.emit('flush');
        await tick();
        if (outcome !== 'refusal') {
          assert.equal(h.writes.length, 1);
          assert.deepEqual(h.completions, []);
          const response = firstResponse as Record<string, unknown>;
          if (outcome === 'external change') {
            h.serverInstance({ ...h.instance(), revision: 8, window_width: 1600, window_height: 900 });
            await h.readiness.refreshInstanceReadiness(instance.id, { retry: false });
            assert.equal(h.store.instances.value[0].revision, 8);
            first.resolve(response);
          } else if (outcome === 'lost reply') first.reject(new Error('Fixture response lost'));
          else if (outcome === 'missing revision') first.resolve({ ...response, revision: undefined });
          else if (outcome === 'fractional revision') first.resolve({ ...response, revision: 7.5 });
          else if (outcome === 'wrong instance') first.resolve({ ...response, id: 'f33cdfc2-1b22-4b6c-8140-ad3cfe8a7104' });
          else first.resolve({ ...response, revision: 8 });
          await tick();
        }
      } finally {
        first.resolve(firstResponse);
        await tick();
        h.dispose();
      }
      assert.deepEqual(h.writes.map((write) => write.patch.expected_revision), outcome === 'external change' ? [6, 7] : [6, 6]);
      assert.deepEqual(h.completions, [{ requestId: 'preferences-1', saved: false }]);
      assert.equal(h.writes.filter((write) => 'max_memory_mb' in write.patch).length, 1, 'the first intent must never replay');
      assert.equal(h.writes.filter((write) => 'window_width' in write.patch).length, 1, 'the queued intent must never replay');
      assert.ok(h.notices.some((notice) => notice.startsWith('Could not save')));
      assert.equal(h.notices.filter((notice) => notice === 'Saved').length, outcome === 'external change' || outcome === 'refusal' ? 1 : 0);
      if (outcome === 'external change') {
        assert.equal(h.instance().revision, 8);
        assert.equal(h.instance().window_width, 1600);
        assert.equal(h.store.instances.value[0].revision, 8, 'the late valid revision 7 reply must not replace the current revision 8 detail');
        assert.equal(h.store.instances.value[0].window_width, 1600);
      } else if (outcome === 'refusal') {
        assert.equal(h.instance().revision, 7);
        assert.equal(h.instance().max_memory_mb, undefined);
        assert.equal(h.instance().window_width, 1000);
      } else {
        assert.equal(h.instance().revision, 7);
        assert.equal(h.instance().max_memory_mb, 5120);
        assert.equal(h.instance().window_width, undefined);
        assert.equal(h.store.instances.value[0].revision, 6, 'an untrusted acknowledgement must not publish');
      }
      assert.equal(h.timers.size, 0);
    });
  }
});

test('instance detail decoding requires a positive lossless integer revision', () => {
  for (const revision of [undefined, null, 0, -1, 1.5, Number.MAX_SAFE_INTEGER + 1, Infinity, NaN, '6']) {
    assert.throws(() => dto.enrichedInstanceResponse({ ...instance, revision }), /Instance revision/);
  }
  assert.equal(dto.enrichedInstanceResponse({ ...instance, revision: 1 }).revision, 1);
  assert.equal(dto.enrichedInstanceResponse({ ...instance, revision: Number.MAX_SAFE_INTEGER }).revision, Number.MAX_SAFE_INTEGER);
});

test('a delayed settings detail cannot replace newer Ready from the download registry', async () => {
  const h = harness();
  const oldReply = deferred<unknown>();
  const busy: EnrichedInstance = {
    ...structuredClone(instance), launchable: false,
    launch_action: { state_id: 'blocked', label: 'Unavailable', tone: 'warn', launchable: false,
      primary_action: 'blocked', disabled_reason: 'The instance is busy. Wait for its current operation to finish.' },
  };
  h.store.instances.value = [busy];
  h.readInstance(() => oldReply.promise);
  try {
    h.render('instance');
    await tick();
    assert.deepEqual(h.reads, [`/instances/${instance.id}`]);
    await h.downloads.refreshInstallQueue({ connectActive: true });
    const ready = h.store.instances.value[0];
    assert.equal(ready.launch_action.label, 'Launch');
    oldReply.resolve(busy);
    await tick();
    assert.equal(h.store.instances.value[0], ready, 'the older detail must not restore Busy after Ready');
    const listReads = (): number => h.reads.filter((path) => path === '/instances').length;
    assert.equal(listReads(), 1);
    h.emitQueue(queue(2));
    await tick();
    assert.equal(listReads(), 1, 'the settled cursor must not need another read to repair stale publication');
    h.downloads.disconnectInstallQueue();
    await h.downloads.refreshInstallQueue({ connectActive: true });
    h.emitQueue(queue(2));
    await tick();
    assert.equal(h.store.instances.value[0], ready);
    assert.equal(listReads(), 1);
    await h.downloads.refreshInstallQueue({ requireInstalledState: true });
    assert.equal(listReads(), 2, 'an explicit required refresh must still request a fresh projection');
    assert.equal(h.store.instances.value[0].launch_action.label, 'Launch');
    assert.equal(h.notices.length, 0);
    assert.equal(h.timers.size, 0);
  } finally {
    oldReply.resolve(busy);
    await tick();
    h.dispose();
  }
});

test('a settings detail failure stays quiet and unmounted or mismatched responses are discarded', async () => {
  for (const outcome of ['failure', 'unmount', 'mismatched']) {
    const h = harness();
    const reply = deferred<unknown>();
    h.readInstance(() => reply.promise);
    h.render('instance');
    await tick();
    const before = h.store.instances.value[0];
    if (outcome === 'unmount') {
      h.dispose();
      reply.resolve({ ...instance, name: 'Late settings detail' });
    } else if (outcome === 'mismatched') reply.resolve({ ...instance, id: 'another-instance' });
    else reply.reject(new Error('Settings detail unavailable'));
    await tick();
    assert.equal(h.store.instances.value[0], before);
    assert.equal(h.notices.length, 0);
    assert.equal(h.timers.size, 0);
    assert.equal(h.reads.length, 1);
    h.dispose();
  }
});

test('unmounting an older settings read cannot clear a newer readiness read or its settlement retry', async () => {
  const h = harness();
  const oldReply = deferred<unknown>();
  h.readInstance(() => oldReply.promise);
  h.render('instance');
  await tick();
  let reads = 0;
  h.readInstance(async (value) => ++reads === 1 ? {
    ...value, launchable: false,
    launch_action: { state_id: 'blocked', label: 'Unavailable', tone: 'warn', launchable: false, primary_action: 'blocked' },
  } : value);
  const current = h.readiness.refreshInstanceReadiness(instance.id);
  try {
    await tick();
    assert.equal(h.store.instances.value[0].launchable, false);
    assert.equal(h.timers.size, 1);
    h.dispose();
    oldReply.resolve({ ...instance, name: 'Obsolete detail' });
    await tick();
    h.timersRun();
    await current;
    assert.equal(h.store.instances.value[0].launch_action.label, 'Launch');
    assert.equal(h.store.instances.value[0].name, instance.name);
    assert.equal(reads, 2);
    assert.equal(h.notices.length, 0);
    assert.equal(h.timers.size, 0);
  } finally {
    oldReply.resolve(instance);
    await tick();
    h.timersRun();
    await current;
    h.dispose();
  }
});

test('mounting settings cannot preempt the active readiness settlement retry', async () => {
  const h = harness();
  let settled = false;
  h.readInstance(async (value) => settled ? value : {
    ...value, launchable: false,
    launch_action: { state_id: 'blocked', label: 'Unavailable', tone: 'warn', launchable: false, primary_action: 'blocked' },
  });
  const current = h.readiness.refreshInstanceReadiness(instance.id);
  try {
    await tick();
    assert.equal(h.store.instances.value[0].launchable, false);
    assert.equal(h.timers.size, 1);
    h.render('instance');
    await tick();
    h.dispose();
    settled = true;
    h.timersRun();
    await current;
    assert.equal(h.store.instances.value[0].launch_action.label, 'Launch');
    assert.equal(h.reads.length, 2, 'the view must leave the active owner to complete its two reads');
    assert.equal(h.notices.length, 0);
    assert.equal(h.timers.size, 0);
  } finally {
    settled = true;
    h.timersRun();
    await current;
    h.dispose();
  }
});

test('remounting settings replaces an abandoned read without letting its cleanup clear the new owner', async () => {
  const h = harness();
  const oldReply = deferred<unknown>();
  const currentReply = deferred<unknown>();
  h.readInstance(() => oldReply.promise);
  h.render('instance');
  await tick();
  h.dispose();
  h.readInstance(() => currentReply.promise);
  try {
    h.render('instance');
    await tick();
    assert.equal(h.reads.length, 2, 'an unmounted view cannot block the new view read');
    oldReply.resolve({ ...instance, name: 'Abandoned detail' });
    await tick();
    currentReply.resolve({ ...instance, name: 'Current detail' });
    await tick();
    assert.equal(h.store.instances.value[0].name, 'Current detail');
    assert.equal(h.notices.length, 0);
    assert.equal(h.timers.size, 0);
  } finally {
    oldReply.resolve(instance);
    currentReply.resolve(instance);
    await tick();
    h.dispose();
  }
});

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
