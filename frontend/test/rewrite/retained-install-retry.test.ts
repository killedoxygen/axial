import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import * as installContract from '../../src/dto-install';
import * as coreContract from '../../src/dto-core';
import * as installItems from '../../src/install-item';
import * as downloadPresenters from '../../src/machines/download-view-models';
import type { InstallQueueActiveViewModel, InstallQueueStateResponse } from '../../src/types-install';

const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const requireDependency = createRequire(resolve(frontend, 'package.json'));
const ts: typeof import('typescript') = requireDependency('typescript');
const jsx = requireDependency('preact/jsx-runtime');
const signals: typeof import('@preact/signals') = requireDependency('@preact/signals');

function source<T>(path: string, imports: Record<string, unknown>): T {
  const filename = resolve(frontend, 'src', path);
  const compiled = ts.transpileModule(readFileSync(filename, 'utf8'), {
    fileName: filename,
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020,
      jsx: ts.JsxEmit.ReactJSX, jsxImportSource: 'preact' },
  });
  const exports = {};
  vm.runInNewContext(compiled.outputText, {
    exports, Error, structuredClone, setTimeout, clearTimeout,
    require(id: string): unknown {
      if (Object.prototype.hasOwnProperty.call(imports, id)) return imports[id];
      throw new Error(`Unreviewed retained retry dependency: ${id}`);
    },
  }, { filename });
  return exports as T;
}

const { signal } = signals;
function deferred<T>() {
  let resolveValue!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((done, fail) => { resolveValue = done; reject = fail; });
  return { promise, resolve: resolveValue, reject };
}
const flush = (): Promise<void> => new Promise((done) => setImmediate(done));
type Node = { type: unknown; props: Record<string, unknown> };
function nodes(value: unknown): Node[] {
  if (Array.isArray(value)) return value.flatMap(nodes);
  if (!value || typeof value !== 'object' || !('props' in value)) return [];
  const node = value as Node;
  return [node, ...nodes(node.props.children)];
}

function retained(overrides: Partial<InstallQueueActiveViewModel> = {}): InstallQueueActiveViewModel {
  return {
    queue_id: 'queue-retained', install_id: 'install-retained', operation_id: 'operation-retained',
    kind: 'loader', title: 'Quilt', label: 'Quilt 0.29.1 for Minecraft 1.20.1', summary: 'Settlement pending',
    install_item: { version_id: 'opaque-installed-version', loader: { component_id: 'org.quiltmc.quilt-loader',
      build_id: 'opaque-retained-build', minecraft_version: '1.20.1', loader_version: '0.29.1' } },
    progress: { phase_id: 'settlement_pending', label: 'Settlement pending', progress_pct: 70,
      terminal: false, failed: false },
    retry_action: { action: 'retry', label: 'Retry settlement', enabled: true },
    ...overrides,
  };
}

function queue(active: InstallQueueActiveViewModel | null = retained(), revision = 1): InstallQueueStateResponse {
  return {
    queue_epoch: 'current-process', revision, registry_revision: 0, active, items: [],
    view_model: { state_id: active ? 'active' : 'idle', status_label: active ? 'Installing' : 'Idle',
      title: 'Downloads', summary: '', queued_count: 0, queued_count_label: 'No queued downloads',
      queued_item_label: 'No items queued', section_title: 'Queue', empty_title: 'Nothing downloading', empty_summary: '' },
  };
}

function harness() {
  const calls: Array<{ method: string; path: string; body?: unknown }> = [];
  const errors: string[] = [];
  let subscriptions = 0;
  let receive: ((value: InstallQueueStateResponse) => void) | undefined;
  let request: (method: string, path: string) => Promise<unknown> = async () => queue(null, 2);
  const store = {
    versions: signal<unknown[]>([]), instances: signal<unknown[]>([]), lastInstanceId: signal(null),
    config: signal(null), launchSessions: signal({}), launchState: signal({ status: 'idle' }),
  };
  const machine = source<typeof import('../../src/machines/downloads')>('machines/downloads.ts', {
    '@preact/signals': signals,
    '../api': { api: async (method: string, path: string, body?: unknown) => {
      calls.push({ method, path, body });
      if (path === '/versions') return { versions: [] };
      if (path === '/instances') return { instances: [], last_instance_id: null };
      return request(method, path);
    } },
    '../utils': { errMessage: String, showError: (message: string) => errors.push(message) },
    '../toast': { toast() {} },
    '../loaders/api': { connectInstallQueueSSE: (onValue: typeof receive) => {
      receive = onValue; subscriptions++; return () => {};
    } },
    '../store': store, '../content-activity': { markContentChanged() {} },
    '../dto-install': installContract, '../dto-core': coreContract,
    '../install-item': installItems, './download-view-models': downloadPresenters,
  });
  const atoms = source<typeof import('../../src/ui/Atoms')>('ui/Atoms.tsx', {
    'preact/jsx-runtime': jsx, './Icons': { Icon: 'Icon' },
  });
  const view = source<typeof import('../../src/views/downloads/DownloadsView')>('views/downloads/DownloadsView.tsx', {
    'preact/jsx-runtime': jsx, '../../ui/Atoms': atoms, '../../ui/Icons': { Icon: 'Icon' },
    '../../ui/DownloadFailureNotice': { DownloadFailureNotice: 'DownloadFailureNotice' },
    '../../hooks/use-now': { useNowTicker: () => 0 }, '../../machines/downloads': machine,
  });
  return {
    machine, calls, errors, store,
    get subscriptions() { return subscriptions; },
    request(next: typeof request) { request = next; },
    load(value: InstallQueueStateResponse) {
      return machine.applyInstallQueueResponse(installContract.installQueueStateResponse(value), { connectActive: true });
    },
    emit(value: InstallQueueStateResponse) {
      assert.ok(receive);
      receive(installContract.installQueueStateResponse(value));
    },
    view() { return nodes(view.DownloadsView()); },
    retryButton(): Node | undefined {
      const button = nodes(view.DownloadsView()).find((node) => node.type === atoms.Button);
      return button ? atoms.Button(button.props) as Node : undefined;
    },
  };
}

test('active retry decoding preserves omission and rejects malformed supplied actions', () => {
  const omitted = retained();
  delete omitted.retry_action;
  const decoded = installContract.installQueueStateResponse(JSON.parse(JSON.stringify(queue(omitted))));
  assert.equal(Object.prototype.hasOwnProperty.call(decoded.active, 'retry_action'), false);
  for (const action of [null, false, {}, { action: 'retry', label: 'Retry settlement', enabled: 'true' },
    { action: 1, label: 'Retry settlement', enabled: true },
    { action: 'retry', label: 'Retry settlement', enabled: true, disabled_reason: 4 }]) {
    assert.throws(() => installContract.installQueueStateResponse({ ...queue(), active: { ...retained(), retry_action: action } }), /Install action/);
  }
  const disabled = installContract.installQueueStateResponse(queue(retained({
    retry_action: { action: 'retry', label: 'Retry settlement', enabled: false, disabled_reason: 'Settlement in progress' },
  })));
  assert.equal(disabled.active?.retry_action?.enabled, false);
  assert.equal(disabled.active?.retry_action?.disabled_reason, 'Settlement in progress');
});

test('Downloads only exposes the backend retry action with an exact install identity', async () => {
  const h = harness();
  const omitted = retained();
  delete omitted.retry_action;
  for (const [index, active] of [omitted, retained({ retry_action: { action: 'cancel', label: 'Cancel', enabled: true } }),
    retained({ install_id: null })].entries()) {
    await h.load(queue(active, index + 1));
    assert.equal(h.retryButton(), undefined);
    await h.machine.retryActiveInstall('install-retained');
  }
  assert.equal(h.calls.filter((call) => call.method === 'POST').length, 0);
  await h.load(queue(retained({ retry_action: { action: 'retry', label: 'Retry settlement', enabled: false,
    disabled_reason: 'Settlement in progress' } }), 10));
  const disabled = h.retryButton();
  assert.ok(disabled);
  assert.equal(disabled.props.disabled, true);
  assert.equal(disabled.props.title, 'Settlement in progress');
  assert.equal(disabled.props.onClick, undefined);
  await h.machine.retryActiveInstall('install-retained');
  assert.equal(h.calls.filter((call) => call.method === 'POST').length, 0);
});

for (const kind of ['vanilla', 'loader'] as const) {
  test(`${kind} retained retry pins its install identity and existing queue request while pending`, async () => {
    const h = harness();
    const active = retained({ kind, install_id: 'exact/install?identity',
      ...(kind === 'vanilla' ? { install_item: { version_id: '1.20.1' } } : {}) });
    await h.load(queue(active));
    const reply = deferred<unknown>();
    h.request(() => reply.promise);
    const button = h.retryButton();
    assert.ok(button);
    assert.equal(button.props.class, 'cp-btn cp-btn--sm cp-btn--secondary');
    assert.equal(nodes(button).find((node) => node.type === 'span')?.props.children, 'Retry settlement');
    (button.props.onClick as () => void)();
    await h.machine.retryActiveInstall(active.install_id!);
    assert.equal(h.machine.activeInstallRetryPending.value, true);
    assert.equal(h.retryButton()?.props.disabled, true);
    const posts = h.calls.filter((call) => call.method === 'POST');
    assert.equal(posts.length, 1);
    assert.equal(posts[0].path, '/install/queue/retry?expected_install_id=exact%2Finstall%3Fidentity');
    assert.deepEqual(posts[0].body, kind === 'loader'
      ? { kind: 'loader', component_id: 'org.quiltmc.quilt-loader', build_id: 'opaque-retained-build' }
      : { kind: 'vanilla', version_id: '1.20.1' });
    reply.resolve(queue(null, 2));
    await flush();
    assert.equal(h.machine.activeInstallRetryPending.value, false);
    assert.equal(h.machine.activeDownload.value, null);
    assert.equal(h.calls.filter((call) => call.path === '/install/queue').length, 0);
  });
}

test('a stale rendered retry cannot target the next active install', async () => {
  const h = harness();
  await h.load(queue());
  const oldButton = h.retryButton();
  assert.ok(oldButton);
  await h.load(queue(retained({ install_id: 'next-install', queue_id: 'next-queue' }), 2));
  (oldButton.props.onClick as () => void)();
  await flush();
  assert.equal(h.calls.filter((call) => call.method === 'POST').length, 0);
  assert.equal(h.machine.activeDownload.value?.installId, 'next-install');
});

for (const failure of ['Response lost', 'Install is busy']) {
  test(`${failure} refreshes authoritative queue state without replaying the retry`, async () => {
    const h = harness();
    await h.load(queue());
    const read = deferred<unknown>();
    h.request(async (method) => {
      if (method === 'POST') throw new Error(failure);
      return read.promise;
    });
    const retry = h.machine.retryActiveInstall('install-retained');
    await flush();
    await h.machine.retryActiveInstall('install-retained');
    assert.equal(h.machine.activeInstallRetryPending.value, true);
    assert.equal(h.machine.activeDownload.value?.installId, 'install-retained');
    assert.equal(h.machine.downloadFailure.value, null);
    assert.equal(h.calls.filter((call) => call.method === 'POST').length, 1);
    assert.equal(h.calls.filter((call) => call.path === '/install/queue').length, 1);
    read.resolve(queue(retained({ retry_action: { action: 'retry', label: 'Retry settlement', enabled: false,
      disabled_reason: 'Settlement in progress' } }), 2));
    await retry;
    assert.equal(h.machine.activeInstallRetryPending.value, false);
    assert.equal(h.retryButton()?.props.disabled, true);
    assert.equal(h.machine.downloadFailure.value, null);
    assert.equal(h.errors.length, 1);
    assert.equal(h.subscriptions, 1);
  });
}

test('failed mutation and reconciliation reads preserve the active install until its stream settles it', async () => {
  const h = harness();
  await h.load(queue());
  h.request(async () => { throw new Error('Connection unavailable'); });
  await h.machine.retryActiveInstall('install-retained');
  assert.equal(h.machine.activeDownload.value?.installId, 'install-retained');
  assert.equal(h.machine.downloadFailure.value, null);
  assert.equal(h.calls.filter((call) => call.method === 'POST').length, 1);
  assert.equal(h.calls.filter((call) => call.path === '/install/queue').length, 1);
  const settled = { ...queue(null, 3), registry_revision: 1 };
  h.emit(settled);
  await flush();
  assert.equal(h.machine.activeDownload.value, null);
  assert.equal(h.calls.filter((call) => call.path === '/instances').length, 2);
  assert.equal(h.calls.filter((call) => call.path === '/versions').length, 2);
  assert.equal(h.calls.filter((call) => call.method === 'POST').length, 1);
});

for (const delivery of ['response', 'reconciliation'] as const) {
  test(`an older ${delivery} cannot replace a newer active install or registry projection`, async () => {
    for (const restart of [false, true]) {
      const h = harness();
      await h.load(queue());
      const oldReply = deferred<unknown>();
      h.request(async (method) => {
        if (delivery === 'reconciliation' && method === 'POST') throw new Error('Response lost');
        return oldReply.promise;
      });
      const retry = h.machine.retryActiveInstall('install-retained');
      await flush();
      h.emit({ ...queue(retained({ install_id: 'next-install', queue_id: 'next-queue' }), restart ? 1 : 3),
        queue_epoch: restart ? 'restarted-process' : 'current-process', registry_revision: 1 });
      await flush();
      const reads = h.calls.filter((call) => call.path === '/instances').length;
      oldReply.resolve({ ...queue(null, 2), registry_revision: 0 });
      await retry;
      assert.equal(h.machine.activeDownload.value?.installId, 'next-install');
      assert.equal(h.machine.downloadFailure.value, null);
      assert.equal(h.calls.filter((call) => call.path === '/instances').length, reads);
      assert.equal(h.calls.filter((call) => call.method === 'POST').length, 1);
    }
  });
}

test('ordinary failure retry and queue removal keep their existing routes and exact items', async () => {
  const h = harness();
  const failed = queue(null);
  failed.latest_failure = {
    failed_at_ms: 1, queue_id: 'failed-queue', install_id: 'failed-install', operation_id: 'failed-operation',
    label: 'Minecraft', install_item: { version_id: '1.20.1' },
    failure_view_model: { state_id: 'failed', title: 'Install failed', tone: 'err', summary: 'Download failed', details: [],
      retry_action: { action: 'retry', label: 'Retry', enabled: true },
      dismiss_action: { action: 'dismiss', label: 'Dismiss', enabled: true } },
  };
  await h.load(failed);
  assert.equal(h.retryButton(), undefined);
  const notice = h.view().find((node) => node.type === 'DownloadFailureNotice');
  assert.ok(notice);
  (notice.props.onRetry as () => void)();
  await flush();
  assert.deepEqual(h.calls.find((call) => call.method === 'POST'), {
    method: 'POST', path: '/install/queue/retry', body: { kind: 'vanilla', version_id: '1.20.1' },
  });
  const queued = queue(null, 3);
  queued.items = [{ queue_id: 'queued/id', state_id: 'queued', kind: 'vanilla', title: 'Minecraft', label: '1.20.1',
    summary: 'Queued', detail: '', position: 1, total: 1, install_item: { version_id: '1.20.1' },
    remove_action: { action: 'remove_from_queue', label: 'Remove', enabled: true } }];
  await h.load(queued);
  h.request(async () => queue(null, 4));
  await h.machine.removeQueuedInstall('queued/id');
  assert.equal(h.calls.find((call) => call.method === 'DELETE')?.path, '/install/queue/queued%2Fid');
  assert.equal(h.machine.downloadQueue.value.items.length, 0);
  assert.equal(h.errors.length, 0);
});
