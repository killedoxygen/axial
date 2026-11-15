import assert from 'node:assert/strict';
import { readFileSync, existsSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, dirname, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import { configView } from './wire';

/** @typedef {[method: string, path: string, body?: any]} ApiCall */
/** @typedef {{ type: unknown, props: Record<string, unknown> }} ViewNode */
/** @typedef {{ Button: { onClick: () => void, disabled?: boolean }, OptionTiles: { options: { value: string }[], onChange: (value: string) => void, value: string, disabled?: boolean } }} Controls */
/**
 * @typedef {{
 *   'settings/PerformanceSection': typeof import('../src/settings/PerformanceSection'),
 *   'instances/detail/performance-mode': typeof import('../src/instances/detail/performance-mode'),
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

test('instance modes preserve inheritance and share one description per mode', async () => {
  const h = viewHarness({ config: { performance_mode: 'custom' } });
  const mode = h.load('instances/detail/performance-mode');
  assert.equal(mode.performanceModeFrom(''), null);
  assert.equal(mode.globalPerformanceMode(), 'custom');
  assert.equal(mode.performanceModeLabel('managed'), 'Managed');
  assert.equal(mode.performanceModeLabel('vanilla'), 'Vanilla');
  assert.equal(mode.performanceModeLabel('custom'), 'Custom');
  assert.deepEqual(Object.keys(mode.performanceModeNotes).sort(), ['custom', 'managed', 'vanilla']);
});
