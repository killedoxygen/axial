import assert from 'node:assert/strict';
import { readFileSync, existsSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, dirname, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';

/** @typedef {Parameters<typeof import('../../src/api').api>} ApiCall */
/** @typedef {{ type: unknown, props: Record<string, unknown> }} ViewNode */
/** @typedef {{ Button: { onClick: () => void, disabled?: boolean }, ChoicePills: { options: { value: string }[], onChange: (value: string) => void, value: string, disabled?: boolean } }} Controls */
/**
 * @typedef {{
 *   'views/settings/PerformanceSection': typeof import('../../src/views/settings/PerformanceSection'),
 *   'views/instance/performance-mode': typeof import('../../src/views/instance/performance-mode'),
 *   'views/settings/PerformanceLabProofHistory': typeof import('../../src/views/settings/PerformanceLabProofHistory'),
 *   'views/settings/PerformanceLabCard': typeof import('../../src/views/settings/PerformanceLabCard'),
 *   'views/settings/PerformanceLabSuiteDrivers': typeof import('../../src/views/settings/PerformanceLabSuiteDrivers'),
 *   'dto-performance': typeof import('../../src/dto-performance'),
 * }} ViewModules
 */

const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const dependencies = createRequire(
  existsSync(resolve(frontend, 'node_modules/esbuild'))
    ? resolve(frontend, 'package.json')
    : resolve(frontend, '../legacy/frontend/package.json'),
);
const { transformSync } = /** @type {typeof import('esbuild')} */ (dependencies('esbuild'));

// Execute the real view functions and DTO decoders without a DOM or shared build
// output. Hooks and primitive boundaries are modeled; this is not visual QA.
/** @param {{ config?: { performance_mode: import('../../src/types-performance').PerformanceMode }, api?: (...args: ApiCall) => Promise<unknown> }} [options] */
function viewHarness({ config = { performance_mode: 'managed' }, api = async () => ({}) } = {}) {
  /** @type {unknown[]} */
  const cells = [];
  /** @type {(() => void | (() => void))[]} */
  const effects = [];
  /** @type {(() => void)[]} */
  const cleanups = [];
  /** @type {ApiCall[]} */
  const calls = [];
  /** @type {Parameters<typeof import('../../src/toast').toast>[]} */
  const notices = [];
  /** @type {string[]} */
  const clipboard = [];
  /** @type {Map<string, { exports: Record<string, unknown> }>} */
  const modules = new Map();
  let cursor = 0;
  const store = {
    config: { value: config },
    devMode: { value: false },
    instances: { value: [] },
    selectedInstanceId: { value: null },
    lastInstanceId: { value: null },
    versionById: () => null,
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
    api: {
      /** @param {ApiCall} args */
      api: async (...args) => {
        calls.push(args);
        return api(...args);
      },
      /** @param {unknown} error */
      isApiError: (error) => error instanceof Error && error.name === 'ApiError' && 'status' in error,
    },
    store,
    toast: {
      /** @param {Parameters<typeof import('../../src/toast').toast>} args */
      toast: (...args) => notices.push(args),
    },
    'ui/Atoms': primitives(['Button', 'Pill', 'Toggle']),
    'ui/ChoicePills': primitives(['ChoicePills']),
    'ui/SettingsSheet': primitives(['SettingRow', 'SettingsSection']),
    'ui/Select': primitives(['SelectField']),
    // Config schema migration is shared-owner work, not mirrored in UI fixtures.
    'dto-core': {
      /** @param {unknown} value */
      configResponse: (value) => value,
    },
  };
  const context = vm.createContext({
    Error,
    URLSearchParams,
    Intl,
    Date,
    Number,
    Set,
    Object,
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
  return {
    store,
    calls,
    notices,
    clipboard,
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

test('Performance settings retain three modes, autosave and failure rollback without Guardian controls', async () => {
  let fail = false;
  const h = viewHarness({
    api: async (_method, _path, patch) => {
      if (fail) throw new Error('Setting is locked.');
      assert.ok(patch && typeof patch === 'object' && 'performance_mode' in patch);
      return { performance_mode: patch.performance_mode };
    },
  });
  const { PerformanceSection } = h.load('views/settings/PerformanceSection');
  let view = h.render(PerformanceSection);
  const choice = control(view, 'ChoicePills');
  assert.deepEqual(
    Array.from(choice.options, (option) => option.value),
    ['managed', 'vanilla', 'custom'],
  );
  assert.equal(nodes(view).filter((node) => node.type === 'SettingRow').length, 1);
  assert.doesNotMatch(visibleText(view), /Guardian|Idle integrity/);
  await h.settle();
  choice.onChange('vanilla');
  assert.equal(control(h.render(PerformanceSection), 'ChoicePills').disabled, true);
  await h.settle();
  assert.equal(h.store.config.value.performance_mode, 'vanilla');
  assert.equal(JSON.stringify(h.calls[0]), JSON.stringify(['PUT', '/config', { performance_mode: 'vanilla' }]));
  view = h.render(PerformanceSection);
  await h.settle();
  fail = true;
  control(view, 'ChoicePills').onChange('custom');
  await h.settle();
  assert.equal(control(h.render(PerformanceSection), 'ChoicePills').value, 'vanilla');
  assert.match(h.notices[h.notices.length - 1][0], /Could not save performance settings: Setting is locked/);
});

test('instance modes preserve inheritance and health passes backend copy through the DTO boundary', async () => {
  const response = {
    health: 'invalid',
    view_model: { tone: 'warn', title: 'Review tuning', detail: 'Backend detail' },
  };
  const h = viewHarness({ config: { performance_mode: 'custom' }, api: async () => response });
  const mode = h.load('views/instance/performance-mode');
  assert.equal(mode.performanceModeFrom(''), null);
  assert.equal(mode.globalPerformanceMode(), 'custom');
  assert.equal(mode.performanceModeLabel('managed'), 'Managed');
  assert.equal(mode.performanceModeLabel('vanilla'), 'Vanilla');
  assert.equal(mode.performanceModeLabel('custom'), 'Custom');
  assert.equal(JSON.stringify(await mode.fetchPerformanceHealth('instance /?')), JSON.stringify(response));
  assert.equal(h.calls[0][1], '/performance/health?instance_id=instance+%2F%3F');
});

/** @returns {import('../../src/types-launch').LaunchProofRecord & { guardian: { label: string } }} */
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
    guardian: { label: 'Guardian legacy data must not render' },
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
  const { LaunchProofHistoryBlock } = h.load('views/settings/PerformanceLabProofHistory');
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
  assert.doesNotMatch(text, /Guardian/);
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
  const { LaunchProofHistoryBlock } = h.load('views/settings/PerformanceLabProofHistory');
  const tree = h.render(LaunchProofHistoryBlock, {
    state: { status: 'error', data: [proofRecord()], error: 'Service unavailable.' },
  });
  assert.match(visibleText(tree), /Showing the last loaded records/);
  control(tree, 'Button').onClick();
  await h.settle();
  assert.equal(h.notices[h.notices.length - 1][0], 'Copy failed: Launch proof could not be copied.');
  assert.equal(h.clipboard.length, 0);
});

test('Performance Lab remains a developer disclosure with all existing blocks', async () => {
  const h = viewHarness();
  const { PerformanceLabCard } = h.load('views/settings/PerformanceLabCard');
  assert.equal(h.render(PerformanceLabCard), null);
  await h.settle();
  assert.equal(h.calls.length, 0);
  h.store.devMode.value = true;
  const closed = h.render(PerformanceLabCard);
  await h.settle();
  assert.equal(h.calls.length, 0);
  control(closed, 'Button', 'Open').onClick();
  const opened = h.render(PerformanceLabCard);
  const blocks = nodes(opened)
    .map((node) => (typeof node.type === 'function' ? node.type.name : undefined))
    .filter(Boolean);
  assert.deepEqual(blocks, [
    'LaunchProofHistoryBlock',
    'BenchmarkMatrixBlock',
    'BenchmarkQualificationPreviewBlock',
    'BenchmarkSuiteDriversBlock',
  ]);
  await h.settle();
  assert.deepEqual(
    h.calls.map((call) => call[1]),
    ['/launch/reports', '/launch/benchmark/matrix', '/launch/benchmark/qualification/family-c-1-12-2/preview'],
  );
});

/** @param {{ id?: string, historical?: boolean, state?: string, pending?: number | null, canResume?: boolean, resumedDriverId?: string | null }} [options] */
function driverResponse({
  id = 'driver',
  historical,
  state = 'stopped',
  pending = null,
  canResume,
  resumedDriverId,
} = {}) {
  return {
    status: 'ok',
    ...(resumedDriverId === undefined ? {} : { resumed_driver_id: resumedDriverId }),
    driver: {
      id,
      state,
      suite_id: 'suite',
      mode: 'release_validation',
      ...(historical === undefined ? {} : { historical }),
      active_session_id: null,
      last_session_id: 'recorded-session',
    },
    suite: {
      suite_id: 'suite',
      run_count: 4,
      launched_run_count: state === 'complete' ? 4 : 2,
      pending_run_index: pending,
    },
    view_model: {
      state_label: historical ? `Historical ${state} (read-only)` : 'Backend stopped',
      state_tone: state === 'complete' ? 'ok' : 'warn',
      can_stop: false,
      can_resume: canResume ?? (!historical && state !== 'complete'),
      can_check_family_c_qualification: true,
    },
  };
}

/** @type {Parameters<typeof import('../../src/views/settings/PerformanceLabSuiteDrivers').BenchmarkSuiteDriversBlock>[0]} */
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

test('benchmark driver actions follow backend permissions while ordinary resume remains available', async () => {
  const driver = driverResponse();
  const h = viewHarness({ api: async (method) => (method === 'POST' ? driver : { status: 'ok', drivers: [driver] }) });
  const { BenchmarkSuiteDriversBlock } = h.load('views/settings/PerformanceLabSuiteDrivers');
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
  assert.doesNotMatch(visibleText(tree), /Recorded pending/);
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

test('historical terminal drivers retain recorded pending evidence without mutation controls and still check qualification', async () => {
  for (const state of ['failed', 'stopped', 'interrupted', 'complete']) {
    const pending = state === 'complete' ? null : 2;
    const driver = driverResponse({ historical: true, state, pending });
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
        missing_summary: 'Historical evidence checked',
        suite_summary: 'Recorded suite',
        evidence_summary: 'Proof incomplete',
      },
    };
    const h = viewHarness({
      api: async (_method, path) =>
        path.endsWith('/suite/drivers') ? { status: 'ok', drivers: [driver] } : qualification,
    });
    const { BenchmarkSuiteDriversBlock } = h.load('views/settings/PerformanceLabSuiteDrivers');
    h.render(BenchmarkSuiteDriversBlock, driverProps);
    await h.settle();
    const tree = h.render(BenchmarkSuiteDriversBlock, driverProps);
    const buttons = nodes(tree)
      .filter((node) => node.type === 'Button')
      .map((node) => visibleText(node).trim());
    assert.deepEqual(buttons, ['Refresh', 'Start', 'Check']);
    assert.ok(visibleText(tree).includes(`Historical ${state} (read-only)`));
    assert.ok(visibleText(tree).includes(pending === null ? 'Recorded pending none' : 'Recorded pending #3'));
    assert.doesNotMatch(visibleText(tree), /Active /);
    control(tree, 'Button', 'Check').onClick();
    await h.settle();
    assert.match(visibleText(h.render(BenchmarkSuiteDriversBlock, driverProps)), /Historical evidence checked/);
    assert.deepEqual(
      h.calls.map((call) => call.slice(0, 2)),
      [
        ['GET', '/launch/benchmark/suite/drivers'],
        ['GET', '/launch/benchmark/qualification/family-c-1-12-2/suite'],
      ],
    );
  }
});

test('historical driver wire flag is validated without requiring or inventing launch intents', () => {
  const { benchmarkSuiteDriverResponse } = viewHarness().load('dto-performance');
  assert.equal(benchmarkSuiteDriverResponse(driverResponse()).driver.historical, false);
  assert.equal(benchmarkSuiteDriverResponse(driverResponse({ historical: false })).driver.historical, false);
  assert.equal(benchmarkSuiteDriverResponse(driverResponse({ historical: true })).driver.historical, true);
  for (const historical of [null, 'true', 1, {}]) {
    const value = driverResponse();
    assert.throws(() => benchmarkSuiteDriverResponse({ ...value, driver: { ...value.driver, historical } }));
  }
});

function continuationFixture() {
  const source = driverResponse({ id: 'recorded-driver', historical: true, pending: 2, canResume: true });
  const successor = driverResponse({ id: 'successor-driver', historical: false, state: 'running', canResume: false });
  successor.view_model.state_label = 'Running successor';
  const linked = {
    ...source,
    resumed_driver_id: successor.driver.id,
    view_model: { ...source.view_model, can_resume: false },
  };
  return { source, successor, linked };
}

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
  return nodes(tree).filter((node) => node.props.class === 'cp-settings-driver-row');
}

test('historical Resume publishes the distinct successor, refreshes its source and guards duplicate and stale clicks', async () => {
  const { source, successor, linked } = continuationFixture();
  const original = JSON.stringify(source);
  const post = deferredResponse();
  const h = viewHarness({
    api: async (method, path) => {
      if (method === 'POST') return post.promise;
      if (path.endsWith('/drivers')) return { status: 'ok', drivers: [source] };
      assert.equal(path, '/launch/benchmark/suite/drivers/recorded-driver');
      return linked;
    },
  });
  const { BenchmarkSuiteDriversBlock } = h.load('views/settings/PerformanceLabSuiteDrivers');
  const render = () => h.render(BenchmarkSuiteDriversBlock, driverProps);
  render();
  await h.settle();
  const resume = control(render(), 'Button', 'Resume');
  resume.onClick();
  resume.onClick();
  assert.equal(control(render(), 'Button', 'Resuming').disabled, true);
  post.resolve(successor);
  await h.settle();
  const tree = render();
  assert.equal(driverRows(tree).length, 2);
  assert.match(visibleText(driverRows(tree)[0]), /Running successor/);
  assert.match(visibleText(driverRows(tree)[1]), /Historical stopped \(read-only\).*Recorded pending #3/);
  assert.equal(
    nodes(tree).some((node) => node.type === 'Button' && visibleText(node).trim() === 'Resume'),
    false,
  );
  resume.onClick();
  await h.settle();
  assert.deepEqual(
    h.calls.map((call) => call.slice(0, 2)),
    [
      ['GET', '/launch/benchmark/suite/drivers'],
      ['POST', '/launch/benchmark/suite/drivers/recorded-driver/resume'],
      ['GET', '/launch/benchmark/suite/drivers/recorded-driver'],
    ],
  );
  assert.equal(JSON.stringify(source), original);
  assert.deepEqual(h.notices, [['Driver resumed']]);
});

test('lost historical Resume responses reconcile through the exact source link without replaying POST', async () => {
  for (const failure of [
    new Error('Connection closed'),
    Object.assign(new Error('Unavailable'), { name: 'ApiError', status: 503 }),
    Object.assign(new Error('Driver task refused'), { name: 'ApiError', status: 409 }),
    Object.assign(new Error('Unclassified refusal'), { name: 'ApiError', status: 422 }),
    { invalid: true },
  ]) {
    const { source, successor, linked } = continuationFixture();
    if (failure instanceof Error && 'status' in failure && failure.status === 409) {
      successor.driver.state = 'failed';
      successor.view_model.state_label = 'Failed successor';
      successor.view_model.can_resume = true;
    }
    const h = viewHarness({
      api: async (method, path) => {
        if (method === 'POST') {
          if (failure instanceof Error) throw failure;
          return failure;
        }
        if (path.endsWith('/drivers')) return { status: 'ok', drivers: [source] };
        if (path.endsWith('/recorded-driver')) return linked;
        assert.equal(path, '/launch/benchmark/suite/drivers/successor-driver');
        return successor;
      },
    });
    const { BenchmarkSuiteDriversBlock } = h.load('views/settings/PerformanceLabSuiteDrivers');
    const render = () => h.render(BenchmarkSuiteDriversBlock, driverProps);
    render();
    await h.settle();
    control(render(), 'Button', 'Resume').onClick();
    await h.settle();
    assert.equal(driverRows(render()).length, 2);
    if (successor.driver.state === 'failed') assert.match(visibleText(driverRows(render())[0]), /Failed successor/);
    assert.deepEqual(
      h.calls.map((call) => call.slice(0, 2)),
      [
        ['GET', '/launch/benchmark/suite/drivers'],
        ['POST', '/launch/benchmark/suite/drivers/recorded-driver/resume'],
        ['GET', '/launch/benchmark/suite/drivers/recorded-driver'],
        ['GET', '/launch/benchmark/suite/drivers/successor-driver'],
      ],
    );
    assert.deepEqual(h.notices, [['Driver resumed']]);
  }
});

test('an absent Resume link and failed status reads stay unconfirmed across refresh and only retry GET', async () => {
  const { source, successor, linked } = continuationFixture();
  let sourceReads = 0;
  let lists = 0;
  const h = viewHarness({
    api: async (method, path) => {
      if (method === 'POST') throw new Error('Connection closed');
      if (path.endsWith('/drivers')) {
        lists += 1;
        return {
          status: 'ok',
          drivers: [lists === 1 ? source : { ...source, view_model: { ...source.view_model, can_resume: false } }],
        };
      }
      if (path.endsWith('/recorded-driver')) {
        sourceReads += 1;
        if (sourceReads === 1) return { ...source, resumed_driver_id: null };
        if (sourceReads === 2) throw new Error('Read unavailable');
        return linked;
      }
      assert.equal(path, '/launch/benchmark/suite/drivers/successor-driver');
      return successor;
    },
  });
  const { BenchmarkSuiteDriversBlock } = h.load('views/settings/PerformanceLabSuiteDrivers');
  const render = () => h.render(BenchmarkSuiteDriversBlock, driverProps);
  render();
  await h.settle();
  const originalResume = control(render(), 'Button', 'Resume');
  originalResume.onClick();
  await h.settle();
  assert.equal(control(render(), 'Button', 'Check status').disabled, false);
  control(render(), 'Button', 'Refresh').onClick();
  await h.settle();
  control(render(), 'Button', 'Check status').onClick();
  await h.settle();
  assert.equal(control(render(), 'Button', 'Check status').disabled, false);
  originalResume.onClick();
  originalResume.onClick();
  await h.settle();
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
  assert.equal(sourceReads, 3);
  assert.equal(driverRows(render()).length, 2);
  assert.equal(h.notices.filter(([message]) => message === 'Driver resumed').length, 1);
  assert.equal(h.notices.filter(([message]) => message === 'Resume outcome unconfirmed. Check status.').length, 2);
});

test('definite Resume refusal is retryable but failed reads cannot erase an already observed successor link', async () => {
  const { source, successor, linked } = continuationFixture();
  const refusal = Object.assign(new Error('Invalid driver request'), { name: 'ApiError', status: 400 });
  let refused = true;
  let sourceReads = 0;
  const h = viewHarness({
    api: async (method, path) => {
      if (method === 'POST') {
        if (refused) throw refusal;
        throw new Error('Connection closed');
      }
      if (path.endsWith('/drivers')) return { status: 'ok', drivers: [source] };
      if (path.endsWith('/recorded-driver')) {
        sourceReads += 1;
        if (sourceReads === 2) throw refusal;
        if (sourceReads === 3) return { ...linked, resumed_driver_id: 'different-driver' };
        return linked;
      }
      assert.equal(path, '/launch/benchmark/suite/drivers/successor-driver');
      if (sourceReads === 1) throw Object.assign(new Error('Unavailable'), { name: 'ApiError', status: 404 });
      return successor;
    },
  });
  const { BenchmarkSuiteDriversBlock } = h.load('views/settings/PerformanceLabSuiteDrivers');
  const render = () => h.render(BenchmarkSuiteDriversBlock, driverProps);
  render();
  await h.settle();
  control(render(), 'Button', 'Resume').onClick();
  await h.settle();
  assert.equal(sourceReads, 0);
  assert.equal(h.notices[0][0], 'Resume failed: Invalid driver request');
  refused = false;
  control(render(), 'Button', 'Resume').onClick();
  await h.settle();
  for (let i = 0; i < 3; i += 1) {
    control(render(), 'Button', 'Check status').onClick();
    await h.settle();
  }
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 2);
  assert.equal(
    h.calls.some(([, path]) => path.endsWith('/different-driver')),
    false,
  );
  assert.equal(driverRows(render()).length, 2);
  assert.equal(h.notices[h.notices.length - 1]?.[0], 'Driver resumed');
});

test('late list reads cannot overwrite an acknowledged historical continuation', async () => {
  const { source, successor, linked } = continuationFixture();
  const refresh = deferredResponse();
  let lists = 0;
  const h = viewHarness({
    api: async (method, path) => {
      if (method === 'POST') return successor;
      if (path.endsWith('/drivers')) return ++lists === 1 ? { status: 'ok', drivers: [source] } : refresh.promise;
      return linked;
    },
  });
  const { BenchmarkSuiteDriversBlock } = h.load('views/settings/PerformanceLabSuiteDrivers');
  const render = () => h.render(BenchmarkSuiteDriversBlock, driverProps);
  render();
  await h.settle();
  const tree = render();
  control(tree, 'Button', 'Refresh').onClick();
  control(tree, 'Button', 'Resume').onClick();
  await h.settle();
  refresh.resolve({ status: 'ok', drivers: [source] });
  await h.settle();
  assert.equal(driverRows(render()).length, 2);
  assert.match(visibleText(driverRows(render())[0]), /Running successor/);
  assert.equal(
    nodes(render()).some((node) => node.type === 'Button' && visibleText(node).trim() === 'Resume'),
    false,
  );
});

test('a held successor read cannot overwrite a newer Refresh and Stop publication', async () => {
  const { source, successor, linked } = continuationFixture();
  successor.view_model.can_stop = true;
  const stopped = {
    ...successor,
    driver: { ...successor.driver, state: 'stopped' },
    view_model: { ...successor.view_model, state_label: 'Stopped successor', can_stop: false, can_resume: true },
  };
  const successorRead = deferredResponse();
  let lists = 0;
  const h = viewHarness({
    api: async (method, path) => {
      if (method === 'POST') {
        if (path.endsWith('/stop')) return stopped;
        throw new Error('Connection closed');
      }
      if (path.endsWith('/drivers')) return { status: 'ok', drivers: ++lists === 1 ? [source] : [successor, linked] };
      if (path.endsWith('/recorded-driver')) return linked;
      assert.equal(path, '/launch/benchmark/suite/drivers/successor-driver');
      return successorRead.promise;
    },
  });
  const { BenchmarkSuiteDriversBlock } = h.load('views/settings/PerformanceLabSuiteDrivers');
  const render = () => h.render(BenchmarkSuiteDriversBlock, driverProps);
  render();
  await h.settle();
  control(render(), 'Button', 'Resume').onClick();
  await h.settle();
  control(render(), 'Button', 'Refresh').onClick();
  await h.settle();
  control(render(), 'Button', 'Stop').onClick();
  await h.settle();
  successorRead.resolve(successor);
  await h.settle();
  assert.match(visibleText(driverRows(render())[0]), /Stopped successor/);
  assert.equal(
    nodes(render()).some((node) => node.type === 'Button' && visibleText(node).trim() === 'Stop'),
    false,
  );
  assert.equal(control(driverRows(render())[0], 'Button', 'Resume').disabled, false);
  assert.equal(h.calls.filter(([method, path]) => method === 'POST' && path.endsWith('/resume')).length, 1);
});

test('a historical POST result is not published until its identity matches the source link', async () => {
  const { source, successor, linked } = continuationFixture();
  const wrong = { ...successor, driver: { ...successor.driver, id: 'unrelated-driver' } };
  wrong.view_model = { ...wrong.view_model, state_label: 'Unrelated driver' };
  const sourceRead = deferredResponse();
  const h = viewHarness({
    api: async (method, path) => {
      if (method === 'POST') return wrong;
      if (path.endsWith('/drivers')) return { status: 'ok', drivers: [source] };
      if (path.endsWith('/recorded-driver')) return sourceRead.promise;
      assert.equal(path, '/launch/benchmark/suite/drivers/successor-driver');
      return successor;
    },
  });
  const { BenchmarkSuiteDriversBlock } = h.load('views/settings/PerformanceLabSuiteDrivers');
  const render = () => h.render(BenchmarkSuiteDriversBlock, driverProps);
  render();
  await h.settle();
  control(render(), 'Button', 'Resume').onClick();
  await h.settle();
  assert.equal(driverRows(render()).length, 1);
  sourceRead.resolve(linked);
  await h.settle();
  assert.equal(driverRows(render()).length, 2);
  assert.doesNotMatch(visibleText(render()), /Unrelated driver/);
  assert.deepEqual(
    h.calls.map((call) => call.slice(0, 2)),
    [
      ['GET', '/launch/benchmark/suite/drivers'],
      ['POST', '/launch/benchmark/suite/drivers/recorded-driver/resume'],
      ['GET', '/launch/benchmark/suite/drivers/recorded-driver'],
      ['GET', '/launch/benchmark/suite/drivers/successor-driver'],
    ],
  );
});

test('a Refresh that fences the successor read but then fails retains read-only status recovery', async () => {
  const { source, successor, linked } = continuationFixture();
  const successorRead = deferredResponse();
  const refresh = deferredResponse();
  let lists = 0;
  let successorReads = 0;
  const h = viewHarness({
    api: async (method, path) => {
      if (method === 'POST') throw new Error('Connection closed');
      if (path.endsWith('/drivers')) return ++lists === 1 ? { status: 'ok', drivers: [source] } : refresh.promise;
      if (path.endsWith('/recorded-driver')) return linked;
      return ++successorReads === 1 ? successorRead.promise : successor;
    },
  });
  const { BenchmarkSuiteDriversBlock } = h.load('views/settings/PerformanceLabSuiteDrivers');
  const render = () => h.render(BenchmarkSuiteDriversBlock, driverProps);
  render();
  await h.settle();
  control(render(), 'Button', 'Resume').onClick();
  await h.settle();
  control(render(), 'Button', 'Refresh').onClick();
  successorRead.resolve(successor);
  await h.settle();
  refresh.resolve({ invalid: true });
  await h.settle();
  assert.equal(driverRows(render()).length, 1);
  assert.equal(control(render(), 'Button', 'Check status').disabled, false);
  assert.equal(h.notices[h.notices.length - 1]?.[0], 'Resume accepted. Check status.');
  control(render(), 'Button', 'Check status').onClick();
  await h.settle();
  assert.equal(driverRows(render()).length, 2);
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
});

test('historical Resume rejects inconsistent source and successor responses without enabling replay', async () => {
  const { source, successor, linked } = continuationFixture();
  for (const [sourceResponse, successorResponse] of [
    [{ ...linked, driver: { ...source.driver, id: 'wrong-source' } }, successor],
    [{ ...linked, driver: { ...source.driver, historical: false } }, successor],
    [{ ...linked, resumed_driver_id: source.driver.id }, successor],
    [linked, { ...successor, driver: { ...successor.driver, id: 'wrong-successor' } }],
    [linked, { ...successor, driver: { ...successor.driver, historical: true } }],
    [linked, { ...successor, driver: { ...successor.driver, id: 123 } }],
  ]) {
    const h = viewHarness({
      api: async (method, path) => {
        if (method === 'POST') return source;
        if (path.endsWith('/drivers')) return { status: 'ok', drivers: [source] };
        return path.endsWith('/recorded-driver') ? sourceResponse : successorResponse;
      },
    });
    const { BenchmarkSuiteDriversBlock } = h.load('views/settings/PerformanceLabSuiteDrivers');
    const render = () => h.render(BenchmarkSuiteDriversBlock, driverProps);
    render();
    await h.settle();
    control(render(), 'Button', 'Resume').onClick();
    await h.settle();
    control(render(), 'Button', 'Check status').onClick();
    await h.settle();
    assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
    assert.equal(driverRows(render()).length, 1);
    assert.equal(
      h.notices.some(([message]) => message === 'Driver resumed'),
      false,
    );
  }
});

test('disposed historical Resume handlers neither publish nor continue reconciliation', async () => {
  for (const stage of ['post', 'source', 'successor']) {
    const { source, successor, linked } = continuationFixture();
    const pending = deferredResponse();
    const h = viewHarness({
      api: async (method, path) => {
        if (method === 'POST') {
          if (stage === 'post') return pending.promise;
          throw new Error('Connection closed');
        }
        if (path.endsWith('/drivers')) return { status: 'ok', drivers: [source] };
        if (path.endsWith('/recorded-driver')) return stage === 'source' ? pending.promise : linked;
        return pending.promise;
      },
    });
    const { BenchmarkSuiteDriversBlock } = h.load('views/settings/PerformanceLabSuiteDrivers');
    const render = () => h.render(BenchmarkSuiteDriversBlock, driverProps);
    render();
    await h.settle();
    control(render(), 'Button', 'Resume').onClick();
    await h.settle();
    const callCount = h.calls.length;
    h.dispose();
    pending.resolve(stage === 'source' ? linked : successor);
    await h.settle();
    assert.equal(h.calls.length, callCount);
    assert.equal(driverRows(render()).length, 1);
    assert.deepEqual(h.notices, []);
  }
});

test('historical continuation links decode omitted and null absence and reject malformed wire values', () => {
  const { benchmarkSuiteDriverResponse } = viewHarness().load('dto-performance');
  const source = driverResponse({ historical: true });
  assert.equal(benchmarkSuiteDriverResponse(source).resumed_driver_id, undefined);
  assert.equal(benchmarkSuiteDriverResponse({ ...source, resumed_driver_id: null }).resumed_driver_id, undefined);
  assert.equal(
    benchmarkSuiteDriverResponse({ ...source, resumed_driver_id: 'successor' }).resumed_driver_id,
    'successor',
  );
  for (const resumed_driver_id of [1, false, {}, []]) {
    assert.throws(() => benchmarkSuiteDriverResponse({ ...source, resumed_driver_id }));
  }
});
