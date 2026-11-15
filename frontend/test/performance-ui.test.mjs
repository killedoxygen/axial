import assert from 'node:assert/strict';
import { readFileSync, existsSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, dirname, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import { configView, enrichedInstance } from './wire';

/** @typedef {[method: string, path: string, body?: any]} ApiCall */
/** @typedef {{ type: unknown, props: Record<string, unknown> }} ViewNode */
/** @typedef {{ Button: { onClick: () => void, disabled?: boolean }, OptionTiles: { options: { value: string }[], onChange: (value: string) => void, value: string, disabled?: boolean } }} Controls */
/**
 * @typedef {{
 *   'settings/PerformanceSection': typeof import('../src/settings/PerformanceSection'),
 *   'instances/detail/performance-mode': typeof import('../src/instances/detail/performance-mode'),
 *   'performance/ProofHistory': typeof import('../src/performance/ProofHistory'),
 *   'performance/SuiteDrivers': typeof import('../src/performance/SuiteDrivers'),
 *   'instances/api': typeof import('../src/instances/api'),
 * }} ViewModules
 */

const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const dependencies = createRequire(resolve(frontend, 'package.json'));
const { transformSync } = /** @type {typeof import('esbuild')} */ (dependencies('esbuild'));

// Execute the real view functions and DTO decoders without a DOM or shared build
// output. Hooks and primitive boundaries are modeled; this is not visual QA.
/** @param {{ config?: Partial<import('../src/generated/ConfigView').ConfigView>, api?: (...args: ApiCall) => Promise<unknown> }} [options] */
function viewHarness({ config = { performance_mode: 'managed' }, api = async () => ({}) } = {}) {
  /** @type {unknown[]} */
  const cells = [];
  /** @type {(() => void | (() => void))[]} */
  const effects = [];
  /** @type {(() => void)[]} */
  const cleanups = [];
  /** @type {ApiCall[]} */
  const calls = [];
  /** @type {Parameters<typeof import('../src/app/toast').toast>[]} */
  const notices = [];
  /** @type {string[]} */
  const clipboard = [];
  /** @type {string[]} */
  const subscriptions = [];
  /** @type {Map<string, { exports: Record<string, unknown> }>} */
  const modules = new Map();
  let cursor = 0;
  /** @param {string} path */
  const state = (path) => load(resolve(frontend, 'src', path));
  const store = {
    get config() {
      return /** @type {typeof import('../src/settings/state')} */ (state('settings/state')).config;
    },
    get instances() {
      return /** @type {typeof import('../src/instances/state')} */ (state('instances/state')).instances;
    },
    get launchSessions() {
      return /** @type {typeof import('../src/launch/state')} */ (state('launch/state')).launchSessions;
    },
  };
  const hooks = {
    /** @template T @param {T | (() => T)} initial @returns {[T, (next: T | ((current: T) => T)) => void]} */
    useState(initial) {
      const index = cursor++;
      if (!(index in cells))
        cells[index] = typeof initial === 'function' ? /** @type {() => T} */ (initial)() : initial;
      return [
        /** @type {T} */ (cells[index]),
        (next) => {
          cells[index] =
            typeof next === 'function'
              ? /** @type {(current: T) => T} */ (next)(/** @type {T} */ (cells[index]))
              : next;
        },
      ];
    },
    /** @template T @param {T} initial @returns {{ current: T }} */
    useRef(initial) {
      const index = cursor++;
      if (!(index in cells)) cells[index] = { current: initial };
      return /** @type {{ current: T }} */ (cells[index]);
    },
    /** @template T @param {() => T} calculate */
    useMemo: (calculate) => calculate(),
    /** @param {() => void | (() => void)} effect @param {unknown[]} deps */
    useEffect(effect, deps) {
      const index = cursor++;
      const previous = cells[index];
      if (!Array.isArray(previous) || deps.some((value, i) => !Object.is(value, previous[i]))) effects.push(effect);
      cells[index] = deps;
    },
  };
  /** @param {string[]} names */
  const primitives = (names) => Object.fromEntries(names.map((name) => [name, name]));
  /** @type {Record<string, unknown>} */
  const mocks = {
    '@preact/signals': dependencies('@preact/signals'),
    'preact/hooks': hooks,
    'preact/jsx-runtime': {
      /** @param {unknown} type @param {Record<string, unknown>} props */
      jsx: (type, props) => ({ type, props }),
      /** @param {unknown} type @param {Record<string, unknown>} props */
      jsxs: (type, props) => ({ type, props }),
      Fragment: 'Fragment',
    },
    'platform/transport': {
      /** @param {ApiCall} args */
      request: async (...args) => {
        calls.push(args);
        return api(...args);
      },
      /** @param {unknown} error */
      isApiError: (error) => error instanceof Error && error.name === 'ApiError' && 'status' in error,
    },
    'platform/events': {
      /** @param {string} path */
      subscribeApiEvents: (path) => {
        subscriptions.push(path);
        return () => {};
      },
    },
    'music/music': { Music: { suppress() {}, unsuppress() {} } },
    'app/sound': { Sound: { init() {} } },
    'app/toast': {
      /** @param {Parameters<typeof import('../src/app/toast').toast>} args */
      toast: (...args) => notices.push(args),
      /** @param {string} message */
      showError: (message) => notices.push([message, 'error']),
      /** @param {unknown} error */
      errMessage: (error) => (error instanceof Error && error.message ? error.message : 'Unknown error'),
    },
    'ui/Atoms': primitives(['Button', 'Pill', 'Toggle']),
    'ui/ChoicePills': primitives(['ChoicePills']),
    'ui/SettingsSheet': primitives(['SettingRow', 'SettingsSection', 'OptionTiles']),
    'ui/Select': primitives(['SelectField']),
  };
  const context = vm.createContext({
    Error,
    URLSearchParams,
    Intl,
    Date,
    Number,
    Set,
    Object,
    window: { setInterval: () => 1, clearInterval() {} },
    navigator: {
      clipboard: {
        /** @param {string} text */
        writeText: async (text) => clipboard.push(text),
      },
    },
  });
  /** @param {string} path @returns {Record<string, unknown>} */
  function load(path) {
    const filename = ['.ts', '.tsx', ''].map((extension) => `${path}${extension}`).find(existsSync);
    assert.ok(filename, `Missing source module: ${path}`);
    const cached = modules.get(filename);
    if (cached) return cached.exports;
    const module = { exports: {} };
    modules.set(filename, module);
    const code = transformSync(readFileSync(filename, 'utf8'), {
      loader: filename.endsWith('.tsx') ? 'tsx' : 'ts',
      format: 'cjs',
      jsx: 'automatic',
      jsxImportSource: 'preact',
      sourcefile: filename,
    }).code;
    /** @param {string} id @returns {unknown} */
    const require = (id) => {
      if (mocks[id]) return mocks[id];
      const target = resolve(dirname(filename), id);
      const local = target.slice(resolve(frontend, 'src').length + 1);
      return mocks[local] ?? load(target);
    };
    vm.runInContext(`(function(require, module, exports) {${code}\n})`, context, { filename })(
      require,
      module,
      module.exports,
    );
    return module.exports;
  }
  store.config.value = { ...configView(), ...config };
  return {
    store,
    calls,
    notices,
    clipboard,
    subscriptions,
    /** @template {keyof ViewModules} T @param {T} path @returns {ViewModules[T]} */
    load: (path) => /** @type {ViewModules[T]} */ (/** @type {unknown} */ (load(resolve(frontend, 'src', path)))),
    /** @template {object} P @param {(props: P) => unknown} component @param {P} [props] */
    render(component, props = /** @type {P} */ ({})) {
      cursor = 0;
      return component(props);
    },
    async settle() {
      for (const effect of effects.splice(0)) {
        const cleanup = effect();
        if (cleanup) cleanups.push(cleanup);
      }
      await new Promise(setImmediate);
    },
    dispose() {
      for (const cleanup of cleanups.splice(0)) cleanup();
    },
  };
}

/** @param {unknown} tree @returns {ViewNode[]} */
function nodes(tree) {
  if (Array.isArray(tree)) return tree.flatMap(nodes);
  if (!tree || typeof tree !== 'object') return [];
  const node = viewNode(tree);
  return [node, ...nodes(node.props.children), ...nodes(node.props.control)];
}

/** @param {unknown} value @returns {ViewNode} */
function viewNode(value) {
  assert.ok(value && typeof value === 'object' && 'type' in value && 'props' in value);
  assert.ok(value.props && typeof value.props === 'object');
  return /** @type {ViewNode} */ (value);
}

/** @param {unknown} tree @returns {string} */
function visibleText(tree) {
  if (Array.isArray(tree)) return tree.map(visibleText).join(' ');
  if (tree === null || tree === undefined || typeof tree === 'boolean') return '';
  if (typeof tree !== 'object') return String(tree);
  const node = viewNode(tree);
  return [node.props.title, visibleText(node.props.children), visibleText(node.props.control)].join(' ');
}

/** @template {keyof Controls} T @param {unknown} tree @param {T} type @param {string} [label] @returns {Controls[T]} */
function control(tree, type, label) {
  const matches = nodes(tree).filter((node) => node.type === type);
  const match = label ? matches.find((node) => visibleText(node).trim() === label) : matches[0];
  assert.ok(match, `Missing ${type} ${label ?? ''}`);
  return /** @type {Controls[T]} */ (/** @type {unknown} */ (match.props));
}

test('Performance settings retain three modes, autosave and failure rollback', async () => {
  let fail = false;
  const h = viewHarness({
    api: async (_method, _path, patch) => {
      if (fail) throw new Error('Setting is locked.');
      assert.ok(patch && typeof patch === 'object' && 'performance_mode' in patch);
      return { performance_mode: patch.performance_mode };
    },
  });
  const { PerformanceSection } = h.load('settings/PerformanceSection');
  let view = h.render(PerformanceSection);
  const choice = control(view, 'OptionTiles');
  assert.deepEqual(
    Array.from(choice.options, (option) => option.value),
    ['managed', 'vanilla', 'custom'],
  );
  assert.equal(nodes(view).filter((node) => node.type === 'SettingRow').length, 1);
  await h.settle();
  choice.onChange('vanilla');
  assert.equal(control(h.render(PerformanceSection), 'OptionTiles').disabled, true);
  await h.settle();
  assert.equal(h.store.config.value?.performance_mode, 'vanilla');
  assert.equal(
    JSON.stringify(h.calls[0]),
    JSON.stringify(['PUT', '/config', { performance_mode: 'vanilla', expected_revision: 1 }]),
  );
  view = h.render(PerformanceSection);
  await h.settle();
  fail = true;
  control(view, 'OptionTiles').onChange('custom');
  await h.settle();
  assert.equal(control(h.render(PerformanceSection), 'OptionTiles').value, 'vanilla');
  assert.match(h.notices[h.notices.length - 1][0], /Could not save performance settings: Setting is locked/);
});

test('instance modes preserve inheritance and health passes backend copy through the DTO boundary', async () => {
  const response = {
    health: 'invalid',
    view_model: { tone: 'warn', title: 'Review tuning', detail: 'Backend detail' },
  };
  const h = viewHarness({ config: { performance_mode: 'custom' }, api: async () => response });
  const mode = h.load('instances/detail/performance-mode');
  assert.equal(mode.performanceModeFrom(''), null);
  assert.equal(mode.globalPerformanceMode(), 'custom');
  assert.equal(mode.performanceModeLabel('managed'), 'Managed');
  assert.equal(mode.performanceModeLabel('vanilla'), 'Vanilla');
  assert.equal(mode.performanceModeLabel('custom'), 'Custom');
  const api = h.load('instances/api');
  assert.equal(JSON.stringify(await api.performanceHealth('instance /?')), JSON.stringify(response));
  assert.equal(h.calls[0][1], '/performance/health?instance_id=instance+%2F%3F');
});

/** @returns {any} */
function proofRecord() {
  return {
    schema: 'launch-proof',
    schema_version: 1,
    session_id: 'session /?',
    instance_id: 'fixture',
    version_id: '1.12.2',
    launched_at: '2026-09-08T11:00:00Z',
    recorded_at: '2026-09-08T11:01:00Z',
    boot_duration_ms: 2300,
    outcome: 'completed',
    scenario: {
      scenario_id: 'fixture',
      performance_mode: 'managed',
      requested_memory_mb: 2048,
      benchmark_mode: 'development',
    },
    device: { tier: 'fixture' },
    view_model: {
      outcome_tone: 'ok',
      outcome_label: 'Completed by backend',
      evidence: { tone: 'info', label: 'Performance applied', detail: 'Backend evidence' },
      comparison: { tone: 'ok', label: 'Faster', detail: 'Matched benchmark samples' },
      resource_budget: { pressure: true, pressure_label: 'Memory pressure', details: ['Backend budget'] },
    },
  };
}

test('proof history preserves neutral evidence, budget, comparison and stable copied JSON', async () => {
  const h = viewHarness({ api: async () => ({ version_id: '1.12.2', comparison: { z: 2, a: 1 } }) });
  const { LaunchProofHistoryBlock } = h.load('performance/ProofHistory');
  const tree = h.render(LaunchProofHistoryBlock, { state: { status: 'ready', data: [proofRecord()] } });
  const text = visibleText(tree);
  for (const label of [
    'Performance applied',
    'Backend evidence',
    'Memory pressure',
    'Backend budget',
    'Faster',
    'Matched benchmark samples',
    'Completed by backend',
  ])
    assert.ok(text.includes(label), label);
  control(tree, 'Button').onClick();
  await h.settle();
  assert.equal(h.calls[0][1], '/launch/reports/session%20%2F%3F');
  assert.equal(h.clipboard[0], '{\n  "comparison": {\n    "a": 1,\n    "z": 2\n  },\n  "version_id": "1.12.2"\n}\n');
});

test('proof copy failures do not expose raw service details and stale history stays visible', async () => {
  const h = viewHarness({
    api: async () => {
      throw new Error('secret-token /private/profile');
    },
  });
  const { LaunchProofHistoryBlock } = h.load('performance/ProofHistory');
  const tree = h.render(LaunchProofHistoryBlock, {
    state: { status: 'error', data: [proofRecord()], error: 'Service unavailable.' },
  });
  assert.match(visibleText(tree), /Showing the last loaded records/);
  control(tree, 'Button').onClick();
  await h.settle();
  assert.equal(h.notices[h.notices.length - 1][0], 'Copy failed: Launch proof could not be copied.');
  assert.equal(h.clipboard.length, 0);
});

/** @param {{ id?: string, state?: string, pending?: number | null, canResume?: boolean, sessionId?: string | null }} [options] */
function driverResponse({ id = 'driver', state = 'stopped', pending = null, canResume, sessionId = null } = {}) {
  return {
    status: 'ok',
    driver: {
      id,
      state,
      suite_id: 'suite',
      mode: 'release_validation',
      active_session_id: sessionId,
      last_session_id: 'recorded-session',
    },
    suite: {
      suite_id: 'suite',
      run_count: 4,
      launched_run_count: state === 'complete' ? 4 : 2,
      pending_run_index: pending,
    },
    view_model: {
      state_label: `Backend ${state}`,
      state_tone: state === 'complete' ? 'ok' : 'warn',
      can_stop: state === 'running' || state === 'waiting',
      can_resume: canResume ?? state !== 'complete',
      can_check_family_c_qualification: true,
    },
  };
}

/** @type {Parameters<typeof import('../src/performance/SuiteDrivers').BenchmarkSuiteDriversBlock>[0]} */
const driverProps = {
  matrixState: {
    status: 'ready',
    data: {
      schema: 'benchmark-matrix',
      schema_version: 1,
      modes: [{ id: 'development', description: 'Fixture', intended_use: 'Testing' }],
      run_types: [],
      profiles: [],
      representative_targets: [],
      limits: { max_payload_bytes: 4096, custom_post_values_allowed: false },
    },
  },
};

/** @param {string} id @returns {import('../src/generated/EnrichedInstance').EnrichedInstance} */
function driverInstance(id) {
  return enrichedInstance({ id, name: id, version_id: '1.21' });
}

/** @param {string} id */
function driverSession(id) {
  return {
    session_id: `session-${id}`,
    instance_id: `instance-${id}`,
    revision: 1,
    launched_at: '2026-10-04T08:00:00Z',
    notice: null,
    outcome: null,
    view_model: {
      state_id: 'running',
      label: 'Playing',
      progress_pct: 100,
      terminal: false,
      playing: true,
      process_live: true,
      can_stop: true,
    },
  };
}

test('Refresh adopts every distinct active driver session through the real launch owner', async () => {
  const rows = ['1', '2', '1'].map((id, index) =>
    driverResponse({ id: `driver-${index}`, state: 'waiting', sessionId: `session-${id}` }),
  );
  let lists = 0;
  const h = viewHarness({
    api: async (method, path) => {
      assert.equal(method, 'GET');
      if (path === '/launch/benchmark/suite/drivers') return { status: 'ok', drivers: ++lists === 1 ? [] : rows };
      if (path === '/launch/session-1/status') return driverSession('1');
      if (path === '/launch/session-2/status') return driverSession('2');
      assert.fail(`Unexpected read ${path}`);
    },
  });
  h.store.instances.value = [driverInstance('instance-1'), driverInstance('instance-2')];
  const { BenchmarkSuiteDriversBlock } = h.load('performance/SuiteDrivers');
  const render = () => h.render(BenchmarkSuiteDriversBlock, driverProps);
  render();
  await h.settle();
  assert.equal(Object.keys(h.store.launchSessions.value).length, 0);
  control(render(), 'Button', 'Refresh').onClick();
  await h.settle();
  assert.deepEqual(Object.keys(h.store.launchSessions.value).sort(), ['instance-1', 'instance-2']);
  assert.equal(h.store.launchSessions.value['instance-1'].viewModel.can_stop, true);
  assert.equal(h.store.launchSessions.value['instance-2'].sessionId, 'session-2');
  assert.deepEqual(h.subscriptions, ['/launch/session-1/events', '/launch/session-2/events']);
  const reads = h.calls.filter(([, path]) => path.endsWith('/status')).length;
  control(render(), 'Button', 'Refresh').onClick();
  await h.settle();
  assert.equal(h.calls.filter(([, path]) => path.endsWith('/status')).length, reads);
  assert.equal(h.subscriptions.length, 2);
  assert.deepEqual(h.notices, []);
});

test('an acknowledged Resume stays successful when its separate session read fails safely', async () => {
  const h = viewHarness({
    api: async (method, path) => {
      if (method === 'POST') return driverResponse({ state: 'running', sessionId: 'session-1', canResume: false });
      if (path === '/launch/benchmark/suite/drivers') return { status: 'ok', drivers: [driverResponse()] };
      assert.equal(path, '/launch/session-1/status');
      throw new Error('private provider detail');
    },
  });
  h.store.instances.value = [driverInstance('instance-1')];
  const { BenchmarkSuiteDriversBlock } = h.load('performance/SuiteDrivers');
  const render = () => h.render(BenchmarkSuiteDriversBlock, driverProps);
  render();
  await h.settle();
  control(render(), 'Button', 'Resume').onClick();
  await h.settle();
  assert.match(visibleText(render()), /Backend running/);
  assert.ok(h.notices.some(([message]) => message === 'Driver resumed'));
  assert.ok(h.notices.some(([message, tone]) => tone === 'error' && /session/.test(message)));
  assert.doesNotMatch(h.notices.flat().join(' '), /Resume failed|private provider detail/);
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
  assert.equal(Object.keys(h.store.launchSessions.value).length, 0);
});

for (const invalidation of ['Refresh', 'Stop', 'unmount']) {
  test(`a pending session read cannot adopt after its driver observation changes: ${invalidation}`, async () => {
    const pending = deferredResponse();
    let lists = 0;
    const h = viewHarness({
      api: async (method, path) => {
        if (method === 'POST') return driverResponse();
        if (path === '/launch/benchmark/suite/drivers')
          return {
            status: 'ok',
            drivers: ++lists === 1 ? [driverResponse({ state: 'waiting', sessionId: 'session-1' })] : [],
          };
        assert.equal(path, '/launch/session-1/status');
        return pending.promise;
      },
    });
    h.store.instances.value = [driverInstance('instance-1')];
    const { BenchmarkSuiteDriversBlock } = h.load('performance/SuiteDrivers');
    const render = () => h.render(BenchmarkSuiteDriversBlock, driverProps);
    render();
    await h.settle();
    assert.ok(
      h.calls.some(([, path]) => path === '/launch/session-1/status'),
      'the session read must be pending',
    );
    if (invalidation === 'unmount') h.dispose();
    else control(render(), 'Button', invalidation).onClick();
    await h.settle();
    pending.resolve(driverSession('1'));
    await h.settle();
    assert.equal(Object.keys(h.store.launchSessions.value).length, 0);
    assert.deepEqual(h.subscriptions, []);
  });
}

test('benchmark driver actions follow backend permissions while ordinary resume remains available', async () => {
  const driver = driverResponse();
  const h = viewHarness({ api: async (method) => (method === 'POST' ? driver : { status: 'ok', drivers: [driver] }) });
  const { BenchmarkSuiteDriversBlock } = h.load('performance/SuiteDrivers');
  h.render(BenchmarkSuiteDriversBlock, driverProps);
  await h.settle();
  const tree = h.render(BenchmarkSuiteDriversBlock, driverProps);
  const buttons = nodes(tree)
    .filter((node) => node.type === 'Button')
    .map((node) => visibleText(node).trim());
  assert.deepEqual(buttons, ['Refresh', 'Start', 'Check', 'Resume']);
  assert.match(visibleText(tree), /Backend stopped/);
  assert.match(visibleText(tree), /2\/4 launched/);
  assert.match(visibleText(tree), /Pending none/);
  assert.equal(control(tree, 'Button', 'Start').disabled, true);
  control(tree, 'Button', 'Resume').onClick();
  await h.settle();
  assert.deepEqual(
    h.calls.map((call) => call.slice(0, 2)),
    [
      ['GET', '/launch/benchmark/suite/drivers'],
      ['POST', '/launch/benchmark/suite/drivers/driver/resume'],
    ],
  );
  assert.equal(h.notices[0][0], 'Driver resumed');
});

test('native terminal drivers retain pending evidence and follow backend qualification permissions', async () => {
  for (const state of ['failed', 'stopped', 'interrupted', 'complete']) {
    const pending = state === 'complete' ? null : 2;
    const driver = driverResponse({ state, pending, canResume: false });
    const qualification = {
      schema: 'axial.launch.benchmark.qualification',
      schema_version: 1,
      status: 'incomplete',
      suite: { present: true, suite_id: 'suite', mode: 'release_validation', run_count: 4 },
      target: { family: 'C', loader: 'Forge', version: '1.12.2', mode: 'release_validation' },
      targets: [],
      view_model: {
        status_label: 'Incomplete',
        status_tone: 'warn',
        target_label: 'Family C',
        suite_label: 'suite',
        schema_label: 'Schema 1',
        missing_summary: 'Suite evidence checked',
        suite_summary: 'Recorded suite',
        evidence_summary: 'Proof incomplete',
      },
    };
    const h = viewHarness({
      api: async (_method, path) =>
        path.endsWith('/suite/drivers') ? { status: 'ok', drivers: [driver] } : qualification,
    });
    const { BenchmarkSuiteDriversBlock } = h.load('performance/SuiteDrivers');
    h.render(BenchmarkSuiteDriversBlock, driverProps);
    await h.settle();
    const tree = h.render(BenchmarkSuiteDriversBlock, driverProps);
    const buttons = nodes(tree)
      .filter((node) => node.type === 'Button')
      .map((node) => visibleText(node).trim());
    assert.deepEqual(buttons, ['Refresh', 'Start', 'Check']);
    assert.ok(visibleText(tree).includes(`Backend ${state}`));
    assert.ok(visibleText(tree).includes(pending === null ? 'Pending none' : 'Pending #3'));
    assert.doesNotMatch(visibleText(tree), /Active /);
    control(tree, 'Button', 'Check').onClick();
    await h.settle();
    assert.match(visibleText(h.render(BenchmarkSuiteDriversBlock, driverProps)), /Suite evidence checked/);
    assert.deepEqual(
      h.calls.map((call) => call.slice(0, 2)),
      [
        ['GET', '/launch/benchmark/suite/drivers'],
        ['GET', '/launch/benchmark/qualification/family-c-1-12-2/suite'],
      ],
    );
  }
});

function deferredResponse() {
  /** @type {(value: unknown) => void} */
  let resolve = () => assert.fail('Deferred response was not initialized');
  const promise = new Promise((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

/** @param {unknown} tree */
function driverRows(tree) {
  return nodes(tree).filter((node) => node.props.class === 'cp-lab-driver-row');
}

test('late list reads cannot overwrite an acknowledged native Resume and the same driver remains stoppable', async () => {
  const stopped = driverResponse();
  const running = driverResponse({ state: 'running', canResume: false });
  const refresh = deferredResponse();
  const post = deferredResponse();
  let lists = 0;
  const h = viewHarness({
    api: async (method, path) => {
      if (method === 'POST') return path.endsWith('/resume') ? post.promise : stopped;
      assert.equal(path, '/launch/benchmark/suite/drivers');
      return ++lists === 1 ? { status: 'ok', drivers: [stopped] } : refresh.promise;
    },
  });
  const { BenchmarkSuiteDriversBlock } = h.load('performance/SuiteDrivers');
  const render = () => h.render(BenchmarkSuiteDriversBlock, driverProps);
  render();
  await h.settle();
  const tree = render();
  control(tree, 'Button', 'Refresh').onClick();
  const resume = control(tree, 'Button', 'Resume');
  resume.onClick();
  resume.onClick();
  assert.equal(control(render(), 'Button', 'Resuming').disabled, true);
  post.resolve(running);
  await h.settle();
  refresh.resolve({ status: 'ok', drivers: [stopped] });
  await h.settle();
  assert.equal(driverRows(render()).length, 1);
  assert.match(visibleText(driverRows(render())[0]), /Backend running/);
  assert.equal(
    nodes(render()).some((node) => node.type === 'Button' && visibleText(node).trim() === 'Resume'),
    false,
  );
  control(render(), 'Button', 'Stop').onClick();
  await h.settle();
  assert.match(visibleText(driverRows(render())[0]), /Backend stopped/);
  assert.equal(control(render(), 'Button', 'Resume').disabled, false);
  assert.equal(h.calls.filter(([method, path]) => method === 'POST' && path.endsWith('/resume')).length, 1);
  assert.equal(h.calls.filter(([method, path]) => method === 'POST' && path.endsWith('/stop')).length, 1);
  assert.deepEqual(h.notices, [['Driver resumed'], ['Driver stopped']]);
});

test('disposed native Resume handlers do not publish a late response', async () => {
  const stopped = driverResponse();
  const running = driverResponse({ state: 'running', canResume: false });
  const pending = deferredResponse();
  const h = viewHarness({
    api: async (method) => (method === 'POST' ? pending.promise : { status: 'ok', drivers: [stopped] }),
  });
  const { BenchmarkSuiteDriversBlock } = h.load('performance/SuiteDrivers');
  const render = () => h.render(BenchmarkSuiteDriversBlock, driverProps);
  render();
  await h.settle();
  control(render(), 'Button', 'Resume').onClick();
  await h.settle();
  h.dispose();
  pending.resolve(running);
  await h.settle();
  assert.equal(h.calls.length, 2);
  assert.equal(driverRows(render()).length, 1);
  assert.match(visibleText(driverRows(render())[0]), /Backend stopped/);
  assert.deepEqual(h.notices, []);
});
