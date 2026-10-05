import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, dirname, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import type { ContentSelection, ModpackFilesPlan } from '../../src/types-content';
import type { InstallQueueStateResponse } from '../../src/types-install';
import type { EnrichedInstance } from '../../src/types-instance';

type ApiCall = Parameters<typeof import('../../src/api').api>;
type PickerProps = Parameters<typeof import('../../src/views/discover/ModpackPicker').ModpackPicker>[0];
type ViewNode = { type: unknown; props: Record<string, unknown> };
const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const dependencies = createRequire(resolve(frontend, 'package.json'));
const ts: typeof import('typescript') = dependencies('typescript');

function deferred<T>() {
  let resolveValue!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((yes, no) => {
    resolveValue = yes;
    reject = no;
  });
  return { promise, resolve: resolveValue, reject };
}

function queue(revision = 1, registryRevision = 0): InstallQueueStateResponse {
  return {
    queue_epoch: 'queue-a',
    revision,
    registry_revision: registryRevision,
    active: null,
    items: [],
    latest_failure: null,
    view_model: {
      state_id: 'empty',
      status_label: `Snapshot ${revision}`,
      title: 'Downloads',
      summary: '',
      queued_count: 0,
      queued_count_label: '0',
      queued_item_label: 'Queued',
      section_title: 'Queue',
      empty_title: 'Empty',
      empty_summary: '',
    },
  };
}

function plan(files = ['a', 'b'], installed: string[] = []): ModpackFilesPlan {
  return {
    canonical_id: 'modrinth:pack',
    version_id: 'pack-version',
    name: 'Example pack',
    minecraft: '1.21.1',
    files: files.map((name) => ({
      selection_id: name,
      filename: `${name}.jar`,
      kind: 'mod',
      title: name,
      identified: true,
      compatible: true,
      installed: installed.includes(name),
    })),
  };
}

function harness() {
  const calls: ApiCall[] = [];
  const notices: unknown[][] = [];
  const io = {
    post: async (_call: ApiCall): Promise<unknown> => queue(),
    queue: async (): Promise<unknown> => queue(),
    plan: async (_path: string): Promise<unknown> => plan(),
  };
  const cells: unknown[] = [];
  const effects = new Map<number, () => void | (() => void)>();
  const cleanups = new Map<number, () => void>();
  let cursor = 0;
  let closed = 0;
  let props: PickerProps = {
    open: true,
    instanceId: 'instance-a',
    canonicalId: 'modrinth:pack',
    onClose: () => {
      closed++;
    },
  };
  const hooks = {
    useState<T>(initial: T | (() => T)): [T, (next: T | ((value: T) => T)) => void] {
      const index = cursor++;
      if (!(index in cells)) cells[index] = typeof initial === 'function' ? (initial as () => T)() : initial;
      return [
        cells[index] as T,
        (next) => {
          cells[index] = typeof next === 'function' ? (next as (value: T) => T)(cells[index] as T) : next;
        },
      ];
    },
    useRef<T>(initial: T): { current: T } {
      const index = cursor++;
      if (!(index in cells)) cells[index] = { current: initial };
      return cells[index] as { current: T };
    },
    useMemo<T>(read: () => T): T {
      return read();
    },
    useEffect(effect: () => void | (() => void), deps: unknown[]): void {
      const index = cursor++;
      const previous = cells[index] as unknown[] | undefined;
      if (!previous || deps.some((value, i) => !Object.is(value, previous[i]))) effects.set(index, effect);
      cells[index] = deps;
    },
  };
  const mocks: Record<string, unknown> = {
    '@preact/signals': dependencies('@preact/signals'),
    'preact/hooks': hooks,
    'preact/jsx-runtime': dependencies('preact/jsx-runtime'),
    api: {
      api: async (...args: ApiCall): Promise<unknown> => {
        calls.push(structuredClone(args));
        if (args[0] === 'POST') return io.post(args);
        assert.equal(args[0], 'GET');
        if (args[1] === '/install/queue') return io.queue();
        if (args[1].startsWith('/content/modpack/files?')) return io.plan(args[1]);
        if (args[1] === '/versions') return { versions: [] };
        if (args[1] === '/instances') return { instances: [], last_instance_id: null };
        throw new Error(`Unexpected content request: ${args[1]}`);
      },
    },
    'loaders/api': { connectInstallQueueSSE: () => () => {} },
    toast: { toast: (...args: unknown[]) => notices.push(args) },
    'ui/Dialog': { showChoice: async () => null },
    'ui-state': { navigate: () => {} },
    'views/instance/instance-actions': { openInstanceFolder: async () => {} },
    'ui/Atoms': { Button: 'Button' },
    'ui/Icons': { Icon: 'Icon' },
    'ui/Modal': { Modal: 'Modal', ModalContent: 'ModalContent' },
  };
  const modules = new Map<string, { exports: object }>();
  const sourceRoot = resolve(frontend, 'src');
  const context = vm.createContext({ Error, URLSearchParams, structuredClone, setTimeout, clearTimeout });
  function load<T extends object>(path: string): T {
    if (mocks[path]) return mocks[path] as T;
    const existing = modules.get(path);
    if (existing) return existing.exports as T;
    const filename = resolve(sourceRoot, path + (path.endsWith('ModpackPicker') ? '.tsx' : '.ts'));
    const module = { exports: {} };
    modules.set(path, module);
    const output = ts.transpileModule(readFileSync(filename, 'utf8'), {
      fileName: filename,
      compilerOptions: {
        module: ts.ModuleKind.CommonJS,
        target: ts.ScriptTarget.ES2020,
        jsx: ts.JsxEmit.ReactJSX,
        jsxImportSource: 'preact',
      },
    });
    vm.runInContext(`(function(require, module, exports) {${output.outputText}\n})`, context, { filename })(
      (id: string): unknown => mocks[id] ?? load(resolve(dirname(filename), id).slice(sourceRoot.length + 1)),
      module,
      module.exports,
    );
    return module.exports as T;
  }
  const client = load<typeof import('../../src/content')>('content');
  const downloads = load<typeof import('../../src/machines/downloads')>('machines/downloads');
  const activity = load<typeof import('../../src/content-activity')>('content-activity');
  const { ModpackPicker } =
    load<typeof import('../../src/views/discover/ModpackPicker')>('views/discover/ModpackPicker');
  function render(next: Partial<PickerProps> = {}): unknown {
    props = { ...props, ...next };
    cursor = 0;
    return ModpackPicker(props);
  }
  return {
    calls,
    notices,
    io,
    client,
    downloads,
    activity,
    load,
    render,
    closed: () => closed,
    async settle(): Promise<unknown> {
      for (let attempt = 0; attempt < 4; attempt++) {
        render();
        for (const [index, effect] of effects) {
          effects.delete(index);
          cleanups.get(index)?.();
          const cleanup = effect();
          if (cleanup) cleanups.set(index, cleanup);
          else cleanups.delete(index);
        }
        await new Promise(setImmediate);
      }
      return render();
    },
    dispose(): void {
      for (const cleanup of cleanups.values()) cleanup();
      downloads.disconnectInstallQueue();
    },
  };
}

for (const readFails of [false, true]) {
  test(`lost mod update batch stops submissions and ${readFails ? 'retains uncertainty when the queue read fails' : 'observes accepted work through the queue'}`, async (t) => {
    const h = harness();
    t.after(h.dispose);
    const mods = h.load<typeof import('../../src/views/instance/mod-actions')>('views/instance/mod-actions');
    const bulk = h.load<typeof import('../../src/views/instance/bulk-actions')>('views/instance/bulk-actions');
    const accepted = queue(1);
    h.io.post = async ([method, path, body]) => {
      assert.equal(method, 'POST');
      assert.equal(path, '/content/install');
      const request = body as { instance_id: string; selections: ContentSelection[]; allow_incompatible: boolean };
      assert.equal(request.instance_id, 'instance-a');
      assert.equal(request.allow_incompatible, false);
      const index = accepted.items.length;
      accepted.items.push({
        queue_id: `accepted-${index}`,
        state_id: 'queued',
        kind: 'content',
        title: 'Mod updates',
        label: 'Mod updates',
        summary: '',
        detail: '',
        position: index + 1,
        total: index + 1,
        install_item: {
          version_id: 'fabric',
          content: {
            instance_id: 'instance-a',
            label: 'Mod updates',
            action: {
              kind: 'install',
              selections: structuredClone(request.selections),
              allow_incompatible: false,
            },
          },
        },
        remove_action: { action: 'remove_from_queue', label: 'Remove', enabled: true },
      });
      for (const item of accepted.items) item.total = accepted.items.length;
      accepted.view_model = {
        ...accepted.view_model,
        state_id: 'queued',
        status_label: 'Queued',
        queued_count: accepted.items.length,
        queued_count_label: String(accepted.items.length),
      };
      accepted.revision++;
      if (index === 1) throw new Error('Admission response lost');
      return structuredClone(accepted);
    };
    h.io.queue = async () => {
      if (readFails) throw new Error('Queue read unavailable');
      return structuredClone(accepted);
    };
    await mods.applyModUpdates(
      { id: 'instance-a' } as EnrichedInstance,
      Array.from({ length: 85 }, (_, index) => ({
        canonical_id: `modrinth:${index}`,
        kind: 'mod',
        current_version_id: 'old',
        latest_version_id: `pinned-${index}`,
        latest_version_number: 'new',
      })),
    );
    const posts = h.calls.filter(([method]) => method === 'POST');
    assert.deepEqual(
      posts.map(([, , body]) => (body as { selections: unknown[] }).selections.length),
      [40, 40],
    );
    assert.equal(h.calls.filter(([, path]) => path === '/install/queue').length, 1);
    assert.deepEqual(
      h.downloads.downloadQueue.value.items.map((item) => item.queue_id),
      readFails ? ['accepted-0'] : ['accepted-0', 'accepted-1'],
    );
    assert.equal(h.downloads.downloadQueue.value.view_model.queued_count, readFails ? 1 : 2);
    assert.equal(h.activity.contentRevision.value, 1);
    for (const [batch, [, , body]] of posts.entries()) {
      assert.deepEqual(
        (body as { selections: unknown[] }).selections,
        Array.from({ length: 40 }, (_, offset) => ({
          canonical_id: `modrinth:${batch * 40 + offset}`,
          kind: 'mod',
          version_id: `pinned-${batch * 40 + offset}`,
        })),
      );
    }
    const state = bulk.resourceMutationState('instance-a');
    assert.equal(state.status, 'error');
    assert.ok(state.status === 'error');
    assert.match(state.error, /Queued 40 of 85 updates.*Admission response lost/);
    assert.equal(
      h.notices.some(([message]) => /updates queued$/.test(String(message))),
      false,
    );
  });
}

function nodes(value: unknown): ViewNode[] {
  if (Array.isArray(value)) return value.flatMap(nodes);
  if (!value || typeof value !== 'object') return [];
  const node = value as ViewNode;
  return [node, ...nodes(node.props.children)];
}

function text(value: unknown): string {
  if (Array.isArray(value)) return value.map(text).join(' ');
  if (value == null || typeof value === 'boolean') return '';
  if (typeof value !== 'object') return String(value);
  return text((value as ViewNode).props.children);
}

function addButton(tree: unknown): ViewNode {
  const button = nodes(tree).find((node) => node.type === 'Button' && /Add selected|Queueing/.test(text(node)));
  assert.ok(button);
  return button;
}

function checks(tree: unknown): ViewNode[] {
  return nodes(tree).filter((node) => node.type === 'input' && node.props.type === 'checkbox');
}

for (const kind of ['install', 'pack', 'uninstall'] as const) {
  test(`${kind} admission loss refreshes the queue and preserves the original rejection without replay`, async (t) => {
    const h = harness();
    t.after(h.dispose);
    const lost = new Error('Admission response lost');
    h.io.post = async () => {
      throw lost;
    };
    h.io.queue = async () => queue(3, 1);
    const submit =
      kind === 'install'
        ? () => h.client.installContent('instance-a', [{ canonical_id: 'modrinth:a', kind: 'mod' }])
        : kind === 'pack'
          ? () =>
              h.client.installModpack('instance-a', 'modrinth:pack', 'pack-version', {
                selectedFileIds: ['a'],
                includeOverrides: false,
              })
          : () => h.client.uninstallContents('instance-a', ['modrinth:a']);
    await assert.rejects(submit(), (error) => error === lost);
    assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
    assert.equal(h.calls.filter(([, path]) => path === '/install/queue').length, 1);
    assert.equal(h.downloads.downloadQueue.value.view_model.status_label, 'Snapshot 3');
    assert.equal(h.notices.length, 0);
  });
}

test('malformed queue acknowledgement reconciles without claiming success, even if the read fails', async (t) => {
  const h = harness();
  t.after(h.dispose);
  h.io.post = async () => ({});
  h.io.queue = async () => {
    throw new Error('Queue read unavailable');
  };
  await assert.rejects(h.client.installModpack('instance-a', 'modrinth:pack'), /Install queue epoch/);
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
  assert.equal(h.calls.filter(([, path]) => path === '/install/queue').length, 1);
  assert.equal(
    h.notices.some(([, kind]) => kind === 'success'),
    false,
  );
});

test('lost pack response rechecks installed files without closing as success or retaining a stale selection', async (t) => {
  const h = harness();
  t.after(h.dispose);
  await h.settle();
  h.io.post = async () => {
    throw new Error('Admission response lost');
  };
  h.io.queue = async () => queue(3, 1);
  h.io.plan = async () => plan(['a', 'b'], ['a', 'b']);
  (addButton(h.render()).props.onClick as () => void)();
  const tree = await h.settle();
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
  assert.equal(h.calls.filter(([, path]) => path === '/install/queue').length, 1);
  assert.equal(checks(tree).length, 0);
  assert.equal(addButton(tree).props.disabled, true);
  assert.equal(h.closed(), 0);
  assert.equal(
    h.notices.some(([, kind]) => kind === 'success'),
    false,
  );
  assert.match(text(tree), /Admission response lost/);
});

test('pack revision refresh preserves customized choices and excludes newly available files', async (t) => {
  const h = harness();
  t.after(h.dispose);
  await h.settle();
  (checks(h.render())[1].props.onChange as () => void)();
  h.io.plan = async () => plan(['a', 'b', 'c']);
  h.activity.markContentChanged();
  const tree = await h.settle();
  assert.deepEqual(
    checks(tree).map((node) => node.props.checked),
    [true, false, false],
  );
  assert.equal(addButton(tree).props.disabled, false);
});

for (const refreshFails of [false, true]) {
  test(`lost admission and queue responses require a fresh pack plan when that read ${refreshFails ? 'fails' : 'finds installed files'}`, async (t) => {
    const h = harness();
    t.after(h.dispose);
    await h.settle();
    const originalClick = addButton(h.render()).props.onClick as () => void;
    const freshPlan = deferred<unknown>();
    h.io.post = async () => {
      throw new Error('Admission response lost');
    };
    h.io.queue = async () => {
      throw new Error('Queue read unavailable');
    };
    h.io.plan = () => freshPlan.promise;
    originalClick();
    let tree = await h.settle();
    assert.equal(h.calls.filter(([, path]) => path.startsWith('/content/modpack/files?')).length, 2);
    assert.equal(addButton(tree).props.disabled, true);
    originalClick();
    assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
    if (refreshFails) freshPlan.reject(new Error('Pack read unavailable'));
    else freshPlan.resolve(plan(['a', 'b'], ['a', 'b']));
    tree = await h.settle();
    assert.equal(addButton(tree).props.disabled, true);
    assert.equal(checks(tree).length, 0);
    assert.match(text(tree), /Admission response lost/);
    assert.equal(h.closed(), 0);
    assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
    assert.equal(h.activity.contentRevision.value, 0);
  });
}

test('pack submission captures the selected files and closes once after its acknowledgement', async (t) => {
  const h = harness();
  t.after(h.dispose);
  await h.settle();
  (checks(h.render())[1].props.onChange as () => void)();
  const admission = deferred<unknown>();
  h.io.post = () => admission.promise;
  const click = addButton(h.render()).props.onClick as () => void;
  click();
  click();
  (checks(h.render())[1].props.onChange as () => void)();
  assert.deepEqual(
    h.calls.filter(([method]) => method === 'POST'),
    [
      [
        'POST',
        '/content/modpack/install',
        {
          instance_id: 'instance-a',
          canonical_id: 'modrinth:pack',
          version_id: 'pack-version',
          selected_file_ids: ['a'],
          include_overrides: false,
        },
      ],
    ],
  );
  admission.resolve(queue());
  await h.settle();
  assert.equal(h.closed(), 1);
});

test('late pack reads and submission completion cannot affect a changed target incarnation', async (t) => {
  const h = harness();
  t.after(h.dispose);
  await h.settle();
  const oldRead = deferred<unknown>();
  const admission = deferred<unknown>();
  h.io.post = () => admission.promise;
  (addButton(h.render()).props.onClick as () => void)();
  h.io.plan = () => oldRead.promise;
  h.render({ instanceId: 'instance-b' });
  await h.settle();
  h.io.plan = async () => plan(['fresh']);
  h.render({ instanceId: 'instance-a' });
  await h.settle();
  oldRead.resolve(plan(['stale']));
  admission.resolve(queue(3, 1));
  const tree = await h.settle();
  assert.equal(h.closed(), 0);
  assert.match(text(tree), /fresh/);
  assert.doesNotMatch(text(tree), /stale/);
  assert.equal(h.calls.filter(([method]) => method === 'POST').length, 1);
});
