import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import * as logContract from '../../src/dto-launch';
import type { EnrichedInstance } from '../../src/types-instance';
import type { LaunchSession } from '../../src/types-launch';

const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const requireDependency = createRequire(resolve(frontend, 'package.json'));
const ts: typeof import('typescript') = requireDependency('typescript');
type PaneProps = Parameters<typeof import('../../src/views/instance/tabs/LogsPane').LogsPane>[0];
type Node = { type: unknown; props: Record<string, unknown> & { children?: unknown } };

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
      encodeURIComponent,
      require(id: string): unknown {
        if (Object.prototype.hasOwnProperty.call(imports, id)) return imports[id];
        throw new Error(`Unreviewed live log dependency: ${id}`);
      },
      ...globals,
    },
    { filename },
  );
  return exports as T;
}

function session(id = 'session-1'): LaunchSession {
  return {
    sessionId: id,
    launchedAt: '2026-09-27T09:00:00Z',
    statusRevision: 1,
    viewModel: {
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

function instance(): EnrichedInstance {
  return {
    id: 'instance-1',
    name: 'Example',
    version_id: '1.20.1',
    created_at: '2026-09-27T09:00:00Z',
    java_selection: { kind: 'inherited' },
    revision: 1,
    version_display: {
      loader_key: 'vanilla',
      loader_label: 'Vanilla',
      minecraft_label: '1.20.1',
      loader_version_label: '',
      loader_detail_label: '',
      summary_label: '1.20.1',
      supports_mods: false,
    },
    launchable: false,
    launch_action: {
      state_id: 'blocked',
      label: 'Unavailable',
      tone: 'warn',
      launchable: false,
      primary_action: 'blocked',
    },
    saves_count: 0,
    mods_count: 0,
    resource_count: 0,
    shader_count: 0,
  };
}

function history(text: string, sequence = 1, truncated = false) {
  return { entries: [{ sequence, source: 'stdout', text, truncated }] };
}

function deferred<T>() {
  let resolveValue!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolveValue = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve: resolveValue, reject };
}

function nodes(value: unknown): Node[] {
  if (Array.isArray(value)) return value.flatMap(nodes);
  if (value && typeof value === 'object' && 'props' in value) {
    const node = value as Node;
    return [node, ...nodes(node.props.children)];
  }
  return [];
}

function node(tree: unknown, type: string): Node {
  const result = nodes(tree).find((entry) => entry.type === type);
  assert.ok(result, `Expected ${type}`);
  return result;
}

function text(value: unknown): string {
  if (typeof value === 'string' || typeof value === 'number') return String(value);
  if (Array.isArray(value)) return value.map(text).join('');
  if (value && typeof value === 'object' && 'props' in value) return text((value as Node).props.children);
  return '';
}

const flush = (): Promise<void> => new Promise((done) => setImmediate(done));

function harness() {
  let cursor = 0;
  let dirty = false;
  const states = new Map<number, unknown>();
  const effects = new Map<number, { dependencies: unknown[]; cleanup?: () => void }>();
  const pendingEffects: Array<() => void> = [];
  const hooks = {
    useState<T>(initial: T): [T, (next: T | ((previous: T) => T)) => void] {
      const index = cursor++;
      if (!states.has(index)) states.set(index, initial);
      return [
        states.get(index) as T,
        (next) => {
          const value = typeof next === 'function' ? (next as (previous: T) => T)(states.get(index) as T) : next;
          if (!Object.is(states.get(index), value)) {
            states.set(index, value);
            dirty = true;
          }
        },
      ];
    },
    useMemo<T>(calculate: () => T): T {
      cursor++;
      return calculate();
    },
    useEffect(effect: () => void | (() => void), dependencies: unknown[]): void {
      const index = cursor++;
      const previous = effects.get(index);
      if (
        previous &&
        dependencies.length === previous.dependencies.length &&
        dependencies.every((value, offset) => Object.is(value, previous.dependencies[offset]))
      )
        return;
      pendingEffects.push(() => {
        previous?.cleanup?.();
        effects.set(index, { dependencies, cleanup: effect() || undefined });
      });
    },
  };
  let nextTimer = 0;
  const intervals = new Map<number, { callback: () => void; delay: number }>();
  const clock = {
    setInterval(callback: () => void, delay: number): number {
      const id = ++nextTimer;
      intervals.set(id, { callback, delay });
      return id;
    },
    clearInterval(id: number): void {
      intervals.delete(id);
    },
  };
  const calls: string[] = [];
  const folders: string[][] = [];
  let refreshes = 0;
  let read: (path: string) => Promise<unknown> = async () => history('Game started');
  const store = { launchSessions: { value: { 'instance-1': session() } as Record<string, LaunchSession> } };
  const logs = source<typeof import('../../src/views/instance/logs')>('views/instance/logs.ts', {
    '../../api': {
      api: async (method: string, path: string) => {
        assert.equal(method, 'GET');
        calls.push(path);
        return read(path);
      },
    },
    '../../dto-launch': logContract,
  });
  const pane = source<typeof import('../../src/views/instance/tabs/LogsPane')>(
    'views/instance/tabs/LogsPane.tsx',
    {
      'preact/hooks': hooks,
      'preact/jsx-runtime': requireDependency('preact/jsx-runtime'),
      '../../../ui/Atoms': { Button: 'Button', Pill: 'Pill' },
      '../../../ui/Select': { SelectField: 'SelectField' },
      '../../../ui/Icons': { Icon: 'Icon' },
      '../../../format': { formatBytes: String, fmtRelative: String },
      '../../../utils': { errMessage: (error: unknown) => (error instanceof Error ? error.message : String(error)) },
      '../../../store': store,
      '../logs': logs,
      '../instance-actions': {
        openInstanceFolder: async (...args: string[]) => {
          folders.push(args);
        },
      },
      '../components/resource-bits': { ResourceEmpty: 'ResourceEmpty', ResourceStatus: 'ResourceStatus' },
      '../components/log-line': { LogLines: 'LogLines' },
    },
    { window: clock },
  );
  const props: PaneProps = {
    inst: instance(),
    processLive: true,
    resources: { status: 'error', data: null, error: 'this instance is in use or unavailable' },
    onRefresh: () => {
      refreshes++;
    },
  };
  function render(): Node {
    let tree!: Node;
    let renders = 0;
    do {
      assert.ok(++renders < 20, 'render effects must settle');
      dirty = false;
      cursor = 0;
      const wrapper = pane.LogsPane(props);
      tree = (wrapper.type as (value: PaneProps) => Node)(props);
      for (const effect of pendingEffects.splice(0)) effect();
    } while (dirty);
    return tree;
  }
  function unmount(): void {
    for (const effect of effects.values()) effect.cleanup?.();
  }
  return {
    props,
    store,
    calls,
    folders,
    logs,
    intervals,
    render,
    unmount,
    read(next: typeof read): void {
      read = next;
    },
    refreshes: () => refreshes,
    poll(): void {
      for (const timer of [...intervals.values()]) timer.callback();
    },
  };
}

test('the active Logs pane shows session output while the file inventory is exclusively held', async () => {
  const h = harness();
  h.render();
  await flush();
  let tree = h.render();
  assert.deepEqual(h.calls, ['/launch/session-1/logs']);
  assert.equal(node(tree, 'LogLines').props.text, 'Game started');
  assert.equal(node(tree, 'SelectField').props.value, 'session:session-1');
  assert.match(JSON.stringify(node(tree, 'SelectField').props.options), /Session output/);
  assert.equal(
    nodes(tree).some((entry) => entry.type === 'ResourceStatus'),
    false,
  );
  assert.equal(text(node(tree, 'Pill')), 'Live');
  assert.equal([...h.intervals.values()][0].delay, 2500);
  h.read(async () => history('New output'));
  h.poll();
  await flush();
  tree = h.render();
  assert.equal(node(tree, 'LogLines').props.text, 'New output');
  const errorsFilter = nodes(tree).find((entry) => entry.type === 'button' && text(entry) === 'Errors');
  assert.ok(errorsFilter);
  (errorsFilter.props.onClick as () => void)();
  assert.equal(node(h.render(), 'LogLines').props.filter, 'errors');
  const refresh = nodes(tree).find((entry) => entry.type === 'Button' && text(entry) === 'Refresh');
  assert.ok(refresh);
  (refresh.props.onClick as () => void)();
  h.render();
  await flush();
  assert.equal(h.refreshes(), 1);
  assert.equal(h.calls.length, 3);
  const folder = nodes(tree).find((entry) => entry.type === 'Button' && text(entry) === 'Open folder');
  assert.ok(folder);
  (folder.props.onClick as () => void)();
  assert.deepEqual(h.folders, [['instance-1', 'logs']]);
  h.unmount();
  assert.equal(h.intervals.size, 0);
});

test('session read errors stay visible and polling can recover to an honestly empty output', async () => {
  const h = harness();
  h.read(async () => {
    throw new Error('Session output unavailable');
  });
  h.render();
  await flush();
  let tree = h.render();
  assert.match(text(tree), /Session output unavailable/);
  assert.equal(
    nodes(tree).some((entry) => entry.type === 'LogLines'),
    false,
  );
  h.read(async () => ({ entries: [] }));
  h.poll();
  await flush();
  tree = h.render();
  assert.match(text(tree), /No session output yet/);
  assert.doesNotMatch(text(tree), /Session output unavailable/);
  h.unmount();
});

test('a replacement session cannot display or poll the superseded session response', async () => {
  const h = harness();
  const old = deferred<unknown>();
  h.read(() => old.promise);
  h.render();
  h.poll();
  h.poll();
  assert.equal(h.calls.length, 1, 'polling shares the in-flight read');
  h.store.launchSessions.value = { 'instance-1': session('session-2') };
  h.read(async () => history('Replacement output'));
  h.render();
  await flush();
  old.resolve(history('Old output'));
  await flush();
  const tree = h.render();
  assert.deepEqual(h.calls, ['/launch/session-1/logs', '/launch/session-2/logs']);
  assert.equal(node(tree, 'LogLines').props.text, 'Replacement output');
  assert.equal(h.intervals.size, 1);
  h.unmount();
});

test('a retained session keeps reading output after the process exits until settlement removes it', async () => {
  const h = harness();
  h.props.processLive = false;
  h.render();
  await flush();
  assert.equal(h.intervals.size, 1);
  assert.equal(
    nodes(h.render()).some((entry) => entry.type === 'Pill'),
    false,
  );
  h.read(async () => history('Final drained output'));
  h.poll();
  await flush();
  assert.equal(node(h.render(), 'LogLines').props.text, 'Final drained output');
  assert.equal(h.calls.length, 2);
  h.unmount();
});

test('settlement returns to file logs and keeps compressed archives selectable without reading them', async () => {
  const h = harness();
  const old = deferred<unknown>();
  h.read(() => old.promise);
  h.render();
  h.store.launchSessions.value = {};
  h.props.processLive = false;
  h.props.resources = {
    status: 'ready',
    data: {
      worlds: [],
      mods: [],
      screenshots: [],
      logs: [
        { name: 'latest.log', size: 15, modified_at: '2026-09-27T09:00:00Z' },
        { name: 'archive.log.gz', size: 20, modified_at: '2026-09-26T09:00:00Z' },
      ],
      worlds_count: 0,
      mods_count: 0,
      screenshots_count: 0,
      logs_count: 2,
    },
  };
  h.read(async () => ({ name: 'latest.log', text: 'File output', size: 15, truncated: false }));
  h.render();
  await flush();
  old.resolve(history('Old session output'));
  await flush();
  let tree = h.render();
  assert.deepEqual(h.calls, ['/launch/session-1/logs', '/instances/instance-1/logs/latest.log']);
  assert.equal(node(tree, 'LogLines').props.text, 'File output');
  assert.equal(node(tree, 'SelectField').props.value, 'latest.log');
  assert.equal(h.intervals.size, 0);
  (node(tree, 'SelectField').props.onChange as (value: string) => void)('archive.log.gz');
  tree = h.render();
  assert.match(text(tree), /compressed log archive/);
  assert.equal(h.calls.length, 2);
  assert.equal(
    nodes(tree).some((entry) => entry.type === 'LogLines'),
    false,
  );
  h.unmount();
});

test('session history uses the existing decoder and discloses retained or truncated output', async () => {
  const h = harness();
  h.read(async () => history('Retained line', 8, true));
  h.render();
  await flush();
  const tree = h.render();
  assert.equal(node(tree, 'LogLines').props.text, 'Retained line [truncated]');
  assert.match(text(tree), /Some session output was truncated/);
  h.read(async () => ({ entries: [history('Later', 3).entries[0], history('Earlier', 2).entries[0]] }));
  await assert.rejects(h.logs.fetchSessionLog('session/encoded'), /not ordered/);
  assert.equal(h.calls[h.calls.length - 1], '/launch/session%2Fencoded/logs');
  h.unmount();
});
