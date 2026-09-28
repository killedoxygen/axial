import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { stripTypeScriptTypes } from 'node:module';
import { resolve, dirname, basename } from 'node:path';
import { createContext, SourceTextModule, SyntheticModule } from 'node:vm';
import test from 'node:test';

/**
 * @typedef {{
 * 'content.ts': typeof import('../../src/content'),
 * 'views/discover/actions.ts': typeof import('../../src/views/discover/actions'),
 * 'views/discover/install-workflow.ts': typeof import('../../src/views/discover/install-workflow'),
 * 'machines/discover-search.ts': typeof import('../../src/machines/discover-search')
 * }} SourceModules
 * @typedef {Parameters<typeof import('../../src/api').api>} ApiCall
 * @typedef {import('../../src/views/discover/actions').AddOutcome} AddOutcome
 * @typedef {import('../../src/types-content').ContentPage} ContentPage
 * @typedef {import('../../src/machines/discover-search').DiscoverSearchRequest} SearchRequest
 * @typedef {Parameters<typeof import('../../src/views/discover/actions').commitInstall>} InstallCall
 */

const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const sourceRoot = resolve(frontend, 'src');
/** @template T @param {T} value @returns {T} */
const plain = (value) => JSON.parse(JSON.stringify(value));

// Execute the real clients, decoders and workflows. Only their external I/O
// boundaries are substituted; no compiled files or shared build output.
/** @template {keyof SourceModules} K @param {K} entry @param {Record<string, Record<string, unknown>>} [stubs] @returns {Promise<SourceModules[K]>} */
async function loadSource(entry, stubs = {}) {
  const context = createContext({ URLSearchParams, setTimeout, clearTimeout });
  /** @type {Map<string, import('node:vm').Module>} */
  const modules = new Map();
  const replacements = new Map(Object.entries(stubs).map(([path, exports]) => [resolve(sourceRoot, path), exports]));
  /** @param {string} path @returns {Promise<import('node:vm').Module>} */
  async function moduleAt(path) {
    const existing = modules.get(path);
    if (existing) return existing;
    const replacement = replacements.get(path);
    const module = replacement
      ? new SyntheticModule(Object.keys(replacement), function () {
          for (const [name, value] of Object.entries(replacement)) this.setExport(name, value);
        }, { context, identifier: path })
      : new SourceTextModule(stripTypeScriptTypes(await readFile(path, 'utf8')), { context, identifier: path });
    modules.set(path, module);
    return module;
  }
  const module = await moduleAt(resolve(sourceRoot, entry));
  await module.link((specifier, parent) => moduleAt(resolve(dirname(parent.identifier), `${specifier}.ts`)));
  await module.evaluate();
  return /** @type {SourceModules[K]} */ (module.namespace);
}

/** @template T @returns {{ promise: Promise<T>, resolve: (value: T | PromiseLike<T>) => void, reject: (reason?: unknown) => void }} */
function deferred() {
  /** @type {(value: T | PromiseLike<T>) => void} */
  let resolve = () => { throw new Error('Deferred promise was not initialized'); };
  /** @type {(reason?: unknown) => void} */
  let reject = () => { throw new Error('Deferred promise was not initialized'); };
  /** @type {Promise<T>} */
  const promise = new Promise((done, fail) => { resolve = done; reject = fail; });
  return { promise, resolve, reject };
}

async function settle() {
  for (let index = 0; index < 8; index += 1) await Promise.resolve();
}

/** @type {import('../../src/types-content').ContentSelection} */
const selection = { canonical_id: 'modrinth:project', kind: 'mod', version_id: 'pinned-version' };
/** @type {import('../../src/types-content').ResolutionPlan} */
const conflictPlan = {
  instance_id: 'instance-a', loader: 'fabric', game_version: '1.21.1', items: [],
  conflicts: [{ canonical_id: selection.canonical_id, kind: 'incompatible', detail: 'Requires another version.' }],
  total_download_bytes: 0,
};

/** @returns {import('../../src/types-install').InstallQueueStateResponse} */
function queueResponse() {
  return {
    queue_epoch: 'queue-1', revision: 1, registry_revision: 0, latest_failure: null,
    active: null, items: [], notice: null, started_install: null,
    view_model: {
      state_id: 'idle', status_label: 'Idle', title: 'Downloads', summary: '', queued_count: 0,
      queued_count_label: '0 queued', queued_item_label: 'items', section_title: 'Queue',
      empty_title: 'No downloads', empty_summary: 'No pending downloads.',
    },
  };
}

/** @param {(...args: ApiCall) => unknown} respond */
async function clientHarness(respond) {
  /** @type {ApiCall[]} */
  const calls = [];
  const client = await loadSource('content.ts', {
    'api.ts': { api: async (/** @type {ApiCall} */ ...args) => { calls.push(plain(args)); return respond(...args); } },
    'machines/downloads.ts': { reconcileUncertainMutation: async () => {} },
  });
  return { client, calls };
}

test('search sends the exact filter, target, sort and pagination values and decodes the page', async () => {
  const page = { items: [], offset: 40, limit: 40, total: 89 };
  const { client, calls } = await clientHarness(() => page);
  assert.deepEqual(plain(await client.searchContent({
    kind: 'mod', query: 'map & compass', loader: 'fabric', gameVersion: '1.21.1',
    category: 'utility', sort: 'downloads', offset: 40, limit: 40, instanceId: 'target/a',
  })), page);
  assert.equal(calls[0][0], 'GET');
  const url = new URL(calls[0][1], 'http://localhost');
  assert.equal(url.pathname, '/content/search');
  assert.deepEqual(Object.fromEntries(url.searchParams), {
    kind: 'mod', query: 'map & compass', loader: 'fabric', game_version: '1.21.1',
    category: 'utility', sort: 'downloads', offset: '40', limit: '40', instance_id: 'target/a',
  });
});

test('malformed content pages fail validation instead of becoming empty search success', async () => {
  const { client } = await clientHarness(() => ({ items: [], offset: 0, limit: 40 }));
  await assert.rejects(client.searchContent({ kind: 'resource_pack' }), /Content total response was invalid/);
});

test('setup planning preserves backend plan identity, expiry, conflicts and exact selections', async () => {
  const response = { plan_id: 'setup-plan-1', expires_at_ms: 123456, selection_id: 'selection-7', plan: conflictPlan };
  const { client, calls } = await clientHarness(() => response);
  /** @type {import('../../src/types-content').TargetRef} */
  const target = { kind: 'draft', loader: 'fabric', game_version: '1.21.1' };
  const result = await client.planInstanceSetup('selection-7', target, [selection]);
  assert.equal(result.plan_id, 'setup-plan-1');
  assert.equal(result.expires_at_ms, 123456);
  assert.deepEqual(plain(result.plan.conflicts), conflictPlan.conflicts);
  assert.deepEqual(calls[0], ['POST', '/instances/setup/plan', {
    selection_id: 'selection-7', target, selections: [selection],
  }]);
});

test('modpack picker sends only selected file IDs and explicit override intent', async () => {
  const { client, calls } = await clientHarness(() => queueResponse());
  await client.installModpack('instance-a', 'modrinth:pack', 'pack-version', {
    selectedFileIds: ['mpf1-file-a', 'mpf1-file-b'], includeOverrides: false,
  });
  assert.deepEqual(calls[0], ['POST', '/content/modpack/install', {
    instance_id: 'instance-a', canonical_id: 'modrinth:pack', version_id: 'pack-version',
    selected_file_ids: ['mpf1-file-a', 'mpf1-file-b'], include_overrides: false,
  }]);
  await client.installModpack('new-instance', 'modrinth:pack', 'pack-version');
  assert.deepEqual(calls[1][2], {
    instance_id: 'new-instance', canonical_id: 'modrinth:pack', version_id: 'pack-version',
    include_overrides: true, selected_file_ids: [],
  });
});

test('update and uninstall clients retain instance identity and pinned version selection', async () => {
  const { client, calls } = await clientHarness((_method, path) => path.endsWith('/updates') ? { updates: [] } : queueResponse());
  await client.checkContentUpdates('instance/a');
  await client.installContent('instance/a', [selection], true);
  await client.uninstallContents('instance/a', ['modrinth:project']);
  assert.deepEqual(calls, [
    ['GET', '/instances/instance%2Fa/content/updates'],
    ['POST', '/content/install', { instance_id: 'instance/a', selections: [selection], allow_incompatible: true }],
    ['POST', '/instances/instance%2Fa/content/uninstall', { canonical_ids: ['modrinth:project'] }],
  ]);
});

/** @param {import('../../src/types-content').ResolutionPlan} [planResponse] @param {import('../../src/types-install').InstallQueueStateResponse | Error} [installResponse] */
async function actionHarness(planResponse = { ...conflictPlan, conflicts: [] }, installResponse = queueResponse()) {
  /** @type {{ method: string, path: string, body?: unknown }[]} */
  const requests = [];
  /** @type {Parameters<typeof import('../../src/machines/downloads').applyInstallQueueResponse>[]} */
  const applied = [];
  /** @type {Parameters<typeof import('../../src/toast').toast>[]} */
  const notices = [];
  const actions = await loadSource('views/discover/actions.ts', {
    'api.ts': { api: async (/** @type {ApiCall} */ ...[method, path, body]) => {
      requests.push(plain({ method, path, body }));
      if (path === '/content/plan') return planResponse;
      if (path === '/content/install') {
        if (installResponse instanceof Error) throw installResponse;
        return installResponse;
      }
      throw new Error(`Unexpected request: ${path}`);
    } },
    'machines/downloads.ts': {
      applyInstallQueueResponse: async (/** @type {Parameters<typeof import('../../src/machines/downloads').applyInstallQueueResponse>} */ ...args) => applied.push(args),
      reconcileUncertainMutation: async () => {},
    },
    'toast.ts': { toast: (/** @type {Parameters<typeof import('../../src/toast').toast>} */ ...args) => notices.push(args) },
    'utils.ts': { errMessage: (/** @type {unknown} */ error) => error instanceof Error ? error.message : String(error) },
    'ui-state.ts': { openCreateModpack: () => { throw new Error('Unexpected create navigation'); } },
  });
  return { actions, requests, applied, notices };
}

test('an incompatible plan requests confirmation without submitting an installation', async () => {
  const { actions, requests, applied } = await actionHarness(conflictPlan);
  const outcome = await actions.addToInstance('instance-a', [selection], 'Example');
  assert.equal(outcome.status, 'needs-confirmation');
  assert.equal(requests.length, 1);
  assert.equal(requests[0].path, '/content/plan');
  assert.equal(applied.length, 0);
});

test('queue admission is reported as queued and delegated to the shared downloads owner', async () => {
  const { actions, requests, applied, notices } = await actionHarness();
  const outcome = await actions.addToInstance('instance-a', [selection], 'Example');
  assert.equal(outcome.status, 'queued');
  assert.deepEqual(requests.map((request) => request.path), ['/content/plan', '/content/install']);
  assert.equal(applied.length, 1);
  assert.deepEqual(plain(applied[0][1]), { showNotice: true, connectActive: true });
  assert.deepEqual(notices, [['Queued Example', 'success']]);
});

test('failed admission does not project installation success', async () => {
  const { actions, applied, notices } = await actionHarness(undefined, new Error('Target is busy.'));
  const outcome = await actions.addToInstance('instance-a', [selection], 'Example');
  assert.equal(outcome.status, 'failed');
  assert.equal(outcome.error, 'Target is busy.');
  assert.equal(applied.length, 0);
  assert.deepEqual(notices, [['Target is busy.', 'error']]);
});

/** @param {Parameters<typeof import('../../src/views/discover/install-workflow').createInstallWorkflow>[0]} actions */
async function workflowHarness(actions) {
  const { createInstallWorkflow } = await loadSource('views/discover/install-workflow.ts');
  const workflow = createInstallWorkflow(actions, () => {});
  workflow.setTarget('instance-a');
  return workflow;
}

test('target changes discard pending conflict confirmation and do not install into the new target', async () => {
  /** @type {InstallCall[]} */
  const commits = [];
  const workflow = await workflowHarness({
    addToInstance: async () => ({ status: 'needs-confirmation', plan: conflictPlan }),
    commitInstall: async (...args) => { commits.push(args); return { status: 'queued' }; },
  });
  await workflow.add([selection], 'Example');
  assert.ok(workflow.snapshot().plan);
  workflow.setTarget('instance-b');
  assert.equal(workflow.snapshot().plan, null);
  assert.equal((await workflow.confirm()).status, 'failed');
  assert.equal(commits.length, 0);
});

test('late plan responses cannot restore confirmation after changing away and back to a target', async () => {
  /** @type {ReturnType<typeof deferred<AddOutcome>>} */
  const response = deferred();
  const workflow = await workflowHarness({ addToInstance: () => response.promise, commitInstall: async () => ({ status: 'queued' }) });
  const first = workflow.add([selection], 'Example');
  workflow.setTarget('instance-b');
  workflow.setTarget('instance-a');
  response.resolve({ status: 'needs-confirmation', plan: conflictPlan });
  assert.equal((await first).status, 'superseded');
  assert.deepEqual(plain(workflow.snapshot()), { busy: false, plan: null });
});

test('same-turn duplicate adds and confirms submit once and preserve captured selections', async () => {
  /** @type {ReturnType<typeof deferred<AddOutcome>>} */
  const planning = deferred();
  /** @type {ReturnType<typeof deferred<AddOutcome>>} */
  const installing = deferred();
  /** @type {InstallCall[]} */
  const commits = [];
  let plans = 0;
  const workflow = await workflowHarness({
    addToInstance: () => { plans += 1; return planning.promise; },
    commitInstall: (...args) => { commits.push(plain(args)); return installing.promise; },
  });
  const selections = [{ ...selection }];
  const first = workflow.add(selections, 'Example');
  assert.equal((await workflow.add(selections, 'Duplicate')).status, 'failed');
  selections[0].version_id = 'changed-version';
  planning.resolve({ status: 'needs-confirmation', plan: conflictPlan });
  await first;
  const confirmation = workflow.confirm();
  assert.equal((await workflow.confirm()).status, 'failed');
  workflow.cancel();
  assert.equal(workflow.snapshot().busy, true);
  assert.equal(plans, 1);
  assert.deepEqual(commits[0], ['instance-a', [selection], 'Example', conflictPlan, true]);
  installing.resolve({ status: 'queued' });
  assert.equal((await confirmation).status, 'queued');
  assert.equal(workflow.snapshot().plan, null);
});

test('accepted work finishing after target change cannot clear the current tray through a queued callback', async () => {
  /** @type {ReturnType<typeof deferred<AddOutcome>>} */
  const admission = deferred();
  const workflow = await workflowHarness({ addToInstance: () => admission.promise, commitInstall: async () => ({ status: 'queued' }) });
  const first = workflow.add([selection], 'Example');
  workflow.setTarget('instance-b');
  admission.resolve({ status: 'queued' });
  assert.equal((await first).status, 'superseded');
});

async function searchHarness() {
  const { createDiscoverSearchLifecycle } = await loadSource('machines/discover-search.ts');
  /** @type {Map<number, () => void>} */
  const callbacks = new Map();
  let next = 0;
  /** @type {import('../../src/machines/discover-search').DiscoverSearchSnapshot} */
  const state = { loadedSearchKey: '', loadedContextKey: '', loadedAt: null, results: [], total: 0, loading: false, loadingMore: false, searchError: null };
  const lifecycle = createDiscoverSearchLifecycle({ read: () => state, update: (patch) => Object.assign(state, patch) }, {
    set: (callback) => { callbacks.set(++next, callback); return next; }, clear: (id) => callbacks.delete(id),
  }, () => 1000);
  const run = () => {
    const nextCallback = callbacks.entries().next().value;
    assert.ok(nextCallback, 'Expected a scheduled search');
    const [id, callback] = nextCallback;
    callbacks.delete(id);
    callback();
  };
  /** @param {string} key @param {string} contextKey @param {SearchRequest['search']} search @returns {SearchRequest} */
  const request = (key, contextKey, search) => ({ key, contextKey, input: { kind: 'mod' }, search, errorMessage: (error) => error instanceof Error ? error.message : String(error) });
  return { state, lifecycle, run, request, callbacks };
}

/** @param {string} canonicalId @param {number} total @returns {ContentPage} */
function searchPage(canonicalId, total) {
  return {
    items: [{ canonical_id: canonicalId, kind: 'mod', provider: 'modrinth', project_id: canonicalId,
      title: canonicalId, author: 'Example', summary: '', downloads: 0, follows: 0,
      categories: [], game_versions: [], loaders: [] }],
    total, offset: 0, limit: 40,
  };
}

test('search context changes hide stale actionable results and ignore old pagination', async () => {
  const { state, lifecycle, run, request } = await searchHarness();
  lifecycle.search(request('a', 'instance-a', async () => searchPage('first-a', 2)));
  run(); await settle();
  /** @type {ReturnType<typeof deferred<ContentPage>>} */
  const page = deferred();
  lifecycle.loadMore({ key: 'a', input: { kind: 'mod', offset: 1 }, search: () => page.promise });
  lifecycle.search(request('b', 'instance-b', async () => searchPage('first-b', 1)));
  assert.deepEqual(plain(state.results), []);
  run(); await settle();
  page.resolve(searchPage('stale-a', 2));
  await settle();
  assert.deepEqual(plain(state.results), searchPage('first-b', 1).items);
  assert.equal(state.loadingMore, false);
});

test('search remount shares a pending request and a forced retry wins over an old response', async () => {
  const { state, lifecycle, run, request, callbacks } = await searchHarness();
  /** @type {ReturnType<typeof deferred<ContentPage>>} */
  const first = deferred();
  const initial = request('a', 'target', () => first.promise);
  lifecycle.search(initial); run();
  lifecycle.search(initial);
  assert.equal(callbacks.size, 0);
  lifecycle.search({ ...initial, force: true, search: async () => searchPage('fresh', 1) });
  run(); await settle();
  first.resolve(searchPage('stale', 1));
  await settle();
  assert.deepEqual(plain(state.results), searchPage('fresh', 1).items);
  assert.equal(state.loading, false);
});

test('discover presentation, filters and target bar retain the baseline source', async () => {
  for (const file of ['DiscoverView.tsx', 'ContentDetailView.tsx', 'TargetBar.tsx', 'shared.tsx', 'markdown.tsx', 'discover.css']) {
    const current = await readFile(resolve(sourceRoot, 'views/discover', file), 'utf8');
    const baseline = await readFile(resolve(sourceRoot, '../../legacy/frontend/src/views/discover', file), 'utf8');
    assert.equal(current, baseline, file);
  }
  const picker = await readFile(resolve(sourceRoot, 'views/discover/ModpackPicker.tsx'), 'utf8');
  const baselinePicker = await readFile(resolve(sourceRoot, '../../legacy/frontend/src/views/discover/ModpackPicker.tsx'), 'utf8');
  const markup = '  return (\n    <Modal';
  const currentMarkup = picker.indexOf(markup);
  const baselineMarkup = baselinePicker.indexOf(markup);
  assert.ok(currentMarkup >= 0 && baselineMarkup >= 0);
  assert.equal(picker.slice(currentMarkup), baselinePicker.slice(baselineMarkup));
});
