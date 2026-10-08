import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import * as contracts from '../../src/dto-contract';
import * as core from '../../src/dto-core';
import * as installs from '../../src/dto-install';
import * as items from '../../src/install-item';
import * as presenters from '../../src/create-presenters';
import * as launchAdapters from '../../src/launch-response-adapters';
import * as downloadViews from '../../src/machines/download-view-models';
import type { InstallQueueStateResponse } from '../../src/types-install';
import type { LaunchSession } from '../../src/types-launch';

const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const requireDependency = createRequire(resolve(frontend, 'package.json'));
const ts: typeof import('typescript') = requireDependency('typescript');
const signals: typeof import('@preact/signals') = requireDependency('@preact/signals');

function source<T>(path: string, imports: Record<string, unknown>, globals: Record<string, unknown> = {}): T {
  const filename = resolve(frontend, 'src', path);
  const compiled = ts.transpileModule(readFileSync(filename, 'utf8'), {
    fileName: filename,
    compilerOptions: { target: ts.ScriptTarget.ES2020, module: ts.ModuleKind.CommonJS },
  });
  const exports = {};
  vm.runInNewContext(compiled.outputText, {
    exports, Error, encodeURIComponent, structuredClone, setTimeout, clearTimeout,
    require(id: string): unknown {
      if (id === '@preact/signals') return signals;
      if (Object.prototype.hasOwnProperty.call(imports, id)) return imports[id];
      throw new Error(`Unreviewed setup ordering dependency: ${id}`);
    },
    ...globals,
  }, { filename });
  return exports as T;
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

const flush = (): Promise<void> => new Promise((done) => setImmediate(done));

function instance(launchable = false) {
  return {
    id: 'fixture-instance', name: 'Fixture pack', version_id: '1.21.4', created_at: '2026-09-27T00:00:00Z',
    java_selection: { kind: 'inherited' },
    revision: 1,
    launchable, install_target: null,
    launch_action: { state_id: launchable ? 'ready' : 'busy', label: launchable ? 'Play' : 'Busy',
      tone: launchable ? 'ok' : 'warn', launchable, primary_action: launchable ? 'launch' : 'blocked' },
    version_display: { loader_key: 'vanilla', loader_label: 'Vanilla', minecraft_label: '1.21.4',
      loader_version_label: '', loader_detail_label: '', summary_label: '1.21.4', supports_mods: false },
    saves_count: 0, mods_count: 0, resource_count: 0, shader_count: 0,
  };
}

function queue(revision: number, registryRevision: number): InstallQueueStateResponse {
  return { queue_epoch: 'setup-queue', revision, registry_revision: registryRevision, active: null, items: [], latest_failure: null,
    view_model: { state_id: 'idle', status_label: 'Idle', title: 'Downloads', summary: 'Ready', queued_count: 0,
      queued_count_label: 'No queued downloads', queued_item_label: 'No items queued', section_title: 'Queue',
      empty_title: 'Nothing downloading', empty_summary: 'Ready' } };
}

function response(snapshot: InstallQueueStateResponse | null = queue(3, 0)) {
  return { ...instance(), view_model: { tone: 'success', summary: 'Setup accepted.', detail: null }, install_queue: snapshot };
}

function queuedSetup(): InstallQueueStateResponse {
  return installs.installQueueStateResponse({
    ...queue(4, 1),
    items: [{ queue_id: 'queued-setup', state_id: 'queued', kind: 'content', title: 'Content', label: 'Fixture pack',
      summary: 'Waiting', detail: '', position: 1, total: 1,
      install_item: { version_id: '1.21.4', content: { instance_id: 'fixture-instance', label: 'Setting up Fixture pack',
        action: { kind: 'install', allow_incompatible: false,
          selections: [{ canonical_id: 'modrinth:fixture', kind: 'mod', version_id: 'v1' }] } } },
      remove_action: { action: 'remove_from_queue', label: 'Remove', enabled: true } }],
    view_model: { ...queue(4, 1).view_model, state_id: 'queued', queued_count: 1, queued_count_label: '1 queued' },
  });
}

function harness(create = false) {
  const requests: string[][] = [];
  const errors: string[] = [];
  const notices: unknown[][] = [];
  const navigation: unknown[] = [];
  const reply = deferred<unknown>();
  let backend = create ? [] : [instance()];
  let currentQueue = queue(3, 0);
  let readInstance: () => Promise<unknown> = async () => backend[0];
  let readInstances: () => Promise<unknown> = async () => ({ instances: backend, last_instance_id: null });
  let subscription: ((snapshot: InstallQueueStateResponse) => void) | undefined;
  const store = {
    config: signals.signal(null), instances: signals.signal(backend.map(core.enrichedInstanceResponse)),
    versions: signals.signal([]), lastInstanceId: signals.signal<string | null>(null),
    launchSessions: signals.signal<Record<string, LaunchSession>>({}),
    launchState: signals.signal({ status: 'idle' }),
  };
  const api = { async api(method: string, path: string) {
    requests.push([method, path]);
    if (method === 'POST') return reply.promise;
    assert.equal(method, 'GET');
    if (path === '/instances') return readInstances();
    if (path === '/instances/fixture-instance') return readInstance();
    if (path === '/versions') return { versions: [] };
    if (path === '/install/queue') return currentQueue;
    throw new Error(`Unexpected setup read: ${path}`);
  } };
  const utils = { errMessage: String, showError: (message: string) => errors.push(message) };
  const toast = { toast: (...args: unknown[]) => notices.push(args) };
  const actions = source<typeof import('../../src/actions')>('actions.ts', {
    './store': store, './launch-response-adapters': launchAdapters,
  });
  const readiness = source<typeof import('../../src/instance-readiness')>('instance-readiness.ts', {
    './api': api, './dto-core': core, './store': store, './utils': utils,
  }, { window: { setTimeout: (callback: () => void) => queueMicrotask(callback) } });
  const downloads = source<typeof import('../../src/machines/downloads')>('machines/downloads.ts', {
    '../api': api, '../utils': utils, '../toast': toast, '../store': store,
    '../loaders/api': { connectInstallQueueSSE(next: typeof subscription) { subscription = next; return () => {}; } },
    '../content-activity': { markContentChanged() {} }, '../dto-install': installs, '../dto-core': core,
    '../install-item': items, './download-view-models': downloadViews,
  });
  const dependencies = {
    './api': api, './actions': actions, './store': store, './create-presenters': presenters,
    './dto-contract': contracts, './dto-core': core, './dto-install': installs,
    './instance-readiness': readiness, './machines/downloads': downloads, './toast': toast, './utils': utils,
    './ui-state': { navigate: (target: unknown) => navigation.push(target) },
  };
  const setup = source<typeof import('../../src/instance-setup')>('instance-setup.ts', dependencies);
  const creation = source<typeof import('../../src/instance-create')>('instance-create.ts', dependencies);
  return {
    requests, errors, notices, navigation, reply, store, downloads, setup, creation,
    ready(): void { backend = [instance(true)]; },
    registered(): void { backend = [instance()]; },
    removed(): void { backend = []; },
    readInstance(next: typeof readInstance): void { readInstance = next; },
    readInstances(next: typeof readInstances): void { readInstances = next; },
    emit(snapshot: InstallQueueStateResponse): void {
      currentQueue = snapshot;
      assert.ok(subscription);
      subscription(snapshot);
    },
  };
}

const createArgs = { name: 'Fixture pack', selectionId: 'minecraft|1.21.4', icon: 'stack', accent: 'verdant', setupPlanId: 'plan-1' };

for (const cursor of ['older', 'equal'] as const) {
  test(`a delayed resume response cannot replace readiness from a newer SSE observation (${cursor} queue cursor)`, async (t) => {
    const h = harness();
    t.after(() => h.downloads.disconnectInstallQueue());
    await h.downloads.refreshInstallQueue({ connectActive: true });
    const work = h.setup.resumeInstanceSetup('fixture-instance');
    h.ready();
    h.emit(queue(4, 1));
    await flush();
    assert.equal(h.store.instances.value[0].launchable, true);
    const published: boolean[] = [];
    const stop = signals.effect(() => { published.push(h.store.instances.value[0].launchable); });
    t.after(stop);
    h.reply.resolve(response(cursor === 'older' ? queue(3, 0) : queue(4, 1)));
    assert.equal(await work, true);
    assert.ok(published.every(Boolean), 'a stale response must never publish Busy after Ready');
    assert.equal(h.requests.filter(([method]) => method === 'POST').length, 1);
    assert.equal(h.requests.filter(([, path]) => path === '/instances/fixture-instance').length, 1);
  });

  test(`a delayed create response cannot duplicate an instance published by SSE (${cursor} queue cursor)`, async (t) => {
    const h = harness(true);
    t.after(() => h.downloads.disconnectInstallQueue());
    await h.downloads.refreshInstallQueue({ connectActive: true });
    const work = h.creation.createInstance(createArgs);
    h.ready();
    h.emit(queue(4, 1));
    await flush();
    const current = h.store.instances.value[0];
    assert.equal(current.launchable, true);
    h.reply.resolve(response(cursor === 'older' ? queue(3, 0) : queue(4, 1)));
    const result = await work;
    assert.equal(result.ok, true);
    assert.equal(result.instance, current);
    assert.equal(h.store.instances.value.length, 1);
    assert.equal(h.store.instances.value[0], current);
    assert.equal(h.requests.filter(([method]) => method === 'POST').length, 1);
    assert.equal(h.navigation.length, 1);
  });
}

test('accepted resume refreshes readiness even when the response has no queue snapshot', async () => {
  const h = harness();
  h.ready();
  const work = h.setup.resumeInstanceSetup('fixture-instance');
  h.reply.resolve(response(null));
  assert.equal(await work, true);
  assert.equal(h.store.instances.value[0].launch_action.label, 'Play');
  assert.deepEqual(h.requests.map(([method]) => method), ['POST', 'GET']);
});

test('failed readiness reads preserve accepted resume and expose the read failure without replaying it', async () => {
  const h = harness();
  h.readInstance(async () => { throw new Error('Read unavailable'); });
  const work = h.setup.resumeInstanceSetup('fixture-instance');
  h.reply.resolve(response(null));
  assert.equal(await work, true);
  assert.equal(h.store.instances.value[0].launchable, false);
  assert.deepEqual(h.requests.map(([method]) => method), ['POST', 'GET', 'GET']);
  assert.equal(h.errors.length, 1);
  assert.match(h.errors[0], /Could not refresh launch availability/);
});

test('an ordinary create response publishes its instance once and navigates to it', async () => {
  const h = harness(true);
  const work = h.creation.createInstance(createArgs);
  h.reply.resolve(response(null));
  assert.equal((await work).ok, true);
  assert.equal(h.store.instances.value.length, 1);
  assert.equal(h.store.instances.value[0].id, 'fixture-instance');
  assert.equal(h.requests.length, 1);
  assert.equal(h.navigation.length, 1);
});

for (const readFailure of [false, true]) {
  test(`a delayed creation cannot resurrect a pristine setup removed through another client's queue${readFailure ? ' when reconciliation fails' : ''}`, async (t) => {
    const h = harness(true);
    t.after(() => h.downloads.disconnectInstallQueue());
    await h.downloads.refreshInstallQueue({ connectActive: true });
    const work = h.creation.createInstance(createArgs);
    h.registered();
    const accepted = queuedSetup();
    h.emit(accepted);
    await flush();
    assert.equal(h.store.instances.value[0].id, 'fixture-instance');
    assert.equal(h.downloads.downloadQueue.value.items[0].remove_action.enabled, true);
    h.removed();
    // The shared SSE projection carries the new registry cursor, not the
    // removing client's response-only removed_instance_id.
    h.emit(queue(6, 3));
    await flush();
    assert.equal(h.store.instances.value.length, 0);
    const published: number[] = [];
    const stop = signals.effect(() => { published.push(h.store.instances.value.length); });
    t.after(stop);
    if (readFailure) h.readInstances(async () => { throw new Error('Registry unavailable'); });
    h.reply.resolve(response(accepted));
    const result = await work;
    assert.equal(result.ok, true, 'creation was accepted before the later removal');
    assert.ok(published.every((length) => length === 0), 'the removed instance must never reappear');
    assert.equal(result.instance, undefined);
    assert.equal(h.navigation.length, 0, 'do not navigate to a deleted instance');
    assert.equal(h.requests.filter(([method]) => method === 'POST').length, 1);
    if (readFailure) assert.ok(h.errors.length + h.notices.filter(([, kind]) => kind === 'error').length > 0);
  });
}

test('a registry read preceding creation does not make later accepted creation look deleted', async (t) => {
  const h = harness(true);
  t.after(() => h.downloads.disconnectInstallQueue());
  await h.downloads.refreshInstallQueue({ connectActive: true });
  const work = h.creation.createInstance(createArgs);
  h.emit(queue(4, 1));
  await flush();
  assert.equal(h.store.instances.value.length, 0);
  h.ready();
  h.reply.resolve(response(queue(3, 0)));
  const result = await work;
  assert.equal(result.ok, true);
  assert.equal(h.store.instances.value.length, 1);
  assert.equal(h.store.instances.value[0].launch_action.label, 'Play');
  assert.equal(result.instance, h.store.instances.value[0]);
  assert.equal(h.navigation.length, 1);
  assert.equal(h.requests.filter(([method]) => method === 'POST').length, 1);
});
