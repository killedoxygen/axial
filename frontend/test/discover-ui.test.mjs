import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { stripTypeScriptTypes } from 'node:module';
import { resolve, dirname, basename } from 'node:path';
import { createContext, SourceTextModule, SyntheticModule } from 'node:vm';
import test from 'node:test';
import * as signals from '@preact/signals';
import * as ui from '../src/ui-state';

/**
 * @typedef {{
 * 'content.ts': typeof import('../src/content'),
 * 'views/discover/actions.ts': typeof import('../src/views/discover/actions'),
 * 'views/discover/fit.ts': typeof import('../src/views/discover/fit'),
 * 'views/discover/install-workflow.ts': typeof import('../src/views/discover/install-workflow'),
 * 'views/discover/make.ts': typeof import('../src/views/discover/make'),
 * 'views/discover/state.ts': typeof import('../src/views/discover/state'),
 * 'machines/discover-search.ts': typeof import('../src/machines/discover-search')
 * }} SourceModules
 * @typedef {Parameters<typeof import('../src/api').api>} ApiCall
 * @typedef {import('../src/views/discover/actions').AddOutcome} AddOutcome
 * @typedef {import('../src/types-content').ContentPage} ContentPage
 * @typedef {import('../src/machines/discover-search').DiscoverSearchRequest} SearchRequest
 * @typedef {Parameters<typeof import('../src/views/discover/actions').commitInstall>} InstallCall
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

/** @type {import('../src/types-content').ContentSelection} */
const selection = { canonical_id: 'modrinth:project', kind: 'mod', version_id: 'pinned-version' };
/** @type {import('../src/types-content').ResolutionPlan} */
const conflictPlan = {
  instance_id: 'instance-a', loader: 'fabric', game_version: '1.21.1', items: [],
  conflicts: [{ canonical_id: selection.canonical_id, kind: 'incompatible', detail: 'Requires another version.' }],
  total_download_bytes: 0,
};

/** @returns {import('../src/types-install').InstallQueueStateResponse} */
function queueResponse() {
  return {
    queue_epoch: 'queue-1', revision: 1, registry_revision: 0, failures: [],
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
  /** @type {import('../src/types-content').TargetRef} */
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

/** @param {import('../src/types-content').ResolutionPlan} [planResponse] @param {import('../src/types-install').InstallQueueStateResponse | Error} [installResponse] */
async function actionHarness(planResponse = { ...conflictPlan, conflicts: [] }, installResponse = queueResponse()) {
  /** @type {{ method: string, path: string, body?: unknown }[]} */
  const requests = [];
  /** @type {Parameters<typeof import('../src/machines/downloads').applyInstallQueueResponse>[]} */
  const applied = [];
  /** @type {Parameters<typeof import('../src/toast').toast>[]} */
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
      applyInstallQueueResponse: async (/** @type {Parameters<typeof import('../src/machines/downloads').applyInstallQueueResponse>} */ ...args) => applied.push(args),
      reconcileUncertainMutation: async () => {},
    },
    'toast.ts': { toast: (/** @type {Parameters<typeof import('../src/toast').toast>} */ ...args) => notices.push(args) },
    'utils.ts': { errMessage: (/** @type {unknown} */ error) => error instanceof Error ? error.message : String(error) },
  });
  return { actions, requests, applied, notices };
}

/** The real staging owner over real signals; only the provider, notices and the tray's own module are stood in. */
async function picksHarness() {
  /** @type {Array<{ args: ApiCall, response: ReturnType<typeof deferred<unknown>> }>} */
  const requests = [];
  /** @type {Parameters<typeof import('../src/toast').toast>[]} */
  const notices = [];
  const tray = signals.signal(/** @type {import('../src/views/discover/state').TrayItem[]} */ ([]));
  const state = {
    tray,
    gameVersion: signals.signal(''),
    loader: signals.signal(''),
    selectionOf: (/** @type {import('../src/views/discover/state').TrayItem} */ { canonical_id, kind, version_id }) =>
      ({ canonical_id, kind, ...(version_id ? { version_id } : {}) }),
    unstage: (/** @type {string} */ id) => { tray.value = tray.value.filter((item) => item.canonical_id !== id); },
  };
  const { staging, recheck } = await loadSource('views/discover/fit.ts', {
    'views/discover/@preact/signals.ts': signals,
    'api.ts': {
      api: (/** @type {ApiCall} */ ...args) => {
        const response = deferred();
        requests.push({ args: plain(args), response });
        return response.promise;
      },
    },
    'machines/downloads.ts': { reconcileUncertainMutation: async () => {} },
    'toast.ts': { toast: (/** @type {Parameters<typeof import('../src/toast').toast>} */ ...args) => notices.push(args) },
    'utils.ts': { errMessage: (/** @type {unknown} */ error) => (error instanceof Error ? error.message : String(error)) },
    'views/discover/state.ts': state,
  });
  /** @param {number} count */
  const asked = async (count) => {
    for (let waited = 0; waited < 200 && requests.length < count; waited += 1) await new Promise((done) => setTimeout(done, 10));
    assert.equal(requests.length, count);
  };
  return { staging, recheck, state, requests, notices, asked };
}

/** @param {string} id @param {import('../src/types-content').ContentKind} kind @returns {import('../src/views/discover/state').TrayItem} */
const pick = (id, kind) => ({ canonical_id: `modrinth:${id}`, kind, title: `Pick ${id}` });

/** @param {string} id @returns {import('../src/types-content').ModpackTarget} */
function packTarget(id) {
  return {
    canonical_id: `modrinth:${id}`,
    version_id: `version-${id}`,
    name: `Pack ${id}`,
    minecraft: '1.21.1',
    loader: 'fabric',
    loader_label: 'Fabric',
    selection_id: `selection-${id}`,
  };
}

/** @param {string} loader @param {string} game @param {string[]} [drops] */
const candidate = (loader, game, drops = []) => ({
  loader, loader_label: loader || 'Vanilla', game_version: game, selection_id: `${loader}|${game}`, summary: '',
  supported_count: 1, total_count: 1 + drops.length, complete: drops.length === 0,
  drops: drops.map((id) => ({ canonical_id: `modrinth:${id}`, title: id })),
});

test('a staged pack is read with the active Discover filters and decides which picks its instance takes', async () => {
  const { staging, state, requests, asked } = await picksHarness();
  state.gameVersion.value = '1.20.1';
  state.loader.value = 'forge';
  state.tray.value = [pick('pack', 'modpack'), pick('shader', 'shader_pack'), pick('mod', 'mod'), pick('needs-mod', 'mod')];
  assert.deepEqual(requests[0].args, ['GET', '/content/modpack/target?id=modrinth%3Apack&game_version=1.20.1&loader=forge']);
  assert.deepEqual([staging.value.fit, staging.value.pending], [null, true]);
  requests[0].response.resolve(packTarget('pack'));
  await asked(2);
  // The pack's own loader and version are planned, never ranked among the versions the picks happen to share.
  assert.deepEqual(requests[1].args, ['POST', '/content/plan', {
    target: { kind: 'draft', loader: 'fabric', game_version: '1.21.1' },
    selections: [
      { canonical_id: 'modrinth:shader', kind: 'shader_pack' },
      { canonical_id: 'modrinth:mod', kind: 'mod' },
      { canonical_id: 'modrinth:needs-mod', kind: 'mod' },
    ],
  }]);
  assert.equal(staging.value.pending, true);
  requests[1].response.resolve({
    loader: 'fabric', game_version: '1.21.1', total_download_bytes: 0,
    items: [{
      canonical_id: 'modrinth:needs-mod', title: 'Needs mod', kind: 'mod', project_id: 'needs-mod', version_id: 'v',
      version_number: '1', filename: 'needs-mod.jar', reason: 'selected', already_installed: false, update: false,
      dependencies: [{ project_id: 'mod', kind: 'required' }],
    }],
    conflicts: [{ canonical_id: 'modrinth:mod', kind: 'unavailable', detail: 'No compatible version.' }],
  });
  await settle();
  const { fit, pending } = staging.value;
  assert.ok(fit);
  assert.deepEqual([pending, fit.loader, fit.loaderLabel, fit.gameVersion], [false, 'fabric', 'Fabric', '1.21.1']);
  assert.deepEqual([...(fit.drops ?? [])], ['modrinth:mod', 'modrinth:needs-mod']);
});

test('picks alone take the pair that keeps the most, and an answer for picks since changed is not shown', async () => {
  const { staging, state, requests, asked } = await picksHarness();
  state.tray.value = [pick('one', 'mod')];
  await asked(1);
  state.tray.value = [pick('one', 'mod'), pick('two', 'resource_pack')];
  await asked(2);
  assert.deepEqual(requests.map(({ args }) => args), [
    ['POST', '/content/compatibility', { selections: [{ canonical_id: 'modrinth:one', kind: 'mod' }] }],
    ['POST', '/content/compatibility', { selections: [
      { canonical_id: 'modrinth:one', kind: 'mod' }, { canonical_id: 'modrinth:two', kind: 'resource_pack' },
    ] }],
  ]);
  requests[0].response.resolve({ candidates: [candidate('forge', '1.20.1')] });
  await settle();
  assert.deepEqual([staging.value.fit, staging.value.pending], [null, true]);
  requests[1].response.resolve({ candidates: [candidate('fabric', '1.21.6', ['two']), candidate('fabric', '1.21.1')] });
  await settle();
  const { fit, pending, apart } = staging.value;
  assert.ok(fit);
  assert.deepEqual([pending, apart, fit.loader, fit.gameVersion, [...(fit.drops ?? [])]], [
    false, false, 'fabric', '1.21.6', ['modrinth:two'],
  ]);
  state.tray.value = [pick('three', 'mod'), pick('four', 'mod')];
  await asked(3);
  requests[2].response.resolve({ candidates: [] });
  await settle();
  assert.deepEqual([staging.value.fit, staging.value.pending, staging.value.apart], [null, false, true]);
});

test('a fit check that failed is not a fit, and asking again replaces it', async () => {
  const { staging, recheck, state, requests, asked } = await picksHarness();
  state.tray.value = [pick('pack', 'modpack'), pick('shader', 'shader_pack')];
  requests[0].response.resolve(packTarget('pack'));
  await asked(2);
  requests[1].response.reject(new Error('Modrinth is unreachable.'));
  await settle();
  assert.deepEqual(
    [staging.value.fit?.drops, staging.value.pending, staging.value.error],
    [null, false, 'Modrinth is unreachable.'],
  );
  recheck();
  assert.deepEqual([staging.value.pending, staging.value.error], [true, null]);
  await asked(3);
  assert.deepEqual(requests[2].args, requests[1].args);
  requests[2].response.resolve({ loader: 'fabric', game_version: '1.21.1', total_download_bytes: 0, items: [], conflicts: [] });
  await settle();
  assert.deepEqual([staging.value.fit?.drops?.size, staging.value.pending, staging.value.error], [0, false, null]);

  state.tray.value = [pick('one', 'mod')];
  await asked(4);
  requests[3].response.reject(new Error('Timed out.'));
  await settle();
  assert.deepEqual(
    [staging.value.fit, staging.value.pending, staging.value.apart, staging.value.error],
    [null, false, false, 'Timed out.'],
  );
  recheck();
  await asked(5);
  requests[4].response.resolve({ candidates: [candidate('fabric', '1.21.1')] });
  await settle();
  assert.deepEqual([staging.value.fit?.gameVersion, staging.value.error], ['1.21.1', null]);
});

test('a pack that cannot be read is dropped from the picks once, silently if it already left them', async () => {
  const { staging, state, requests, notices } = await picksHarness();
  state.tray.value = [pick('gone', 'modpack'), pick('broken', 'modpack')];
  assert.equal(requests.length, 2);
  state.tray.value = [pick('broken', 'modpack')];
  requests[0].response.reject(new Error('Old pack failed'));
  await settle();
  assert.deepEqual(notices, []);
  requests[1].response.reject(new Error('Target unavailable'));
  await settle();
  assert.deepEqual(notices, [['Pick broken could not be read: Target unavailable', 'error']]);
  assert.deepEqual(state.tray.value, []);
  assert.deepEqual([staging.value.packs, staging.value.pending], [[], false]);
});

test('a pack opens with the picks it takes, and picks alone or a plain create leave no pack behind', (t) => {
  t.after(() => ui.closeCreate());
  const extras = [{ canonical_id: 'modrinth:mod', kind: /** @type {const} */ ('mod'), title: 'A mod' }];
  ui.openCreateModpack(packTarget('a'), extras);
  assert.deepEqual([plain(ui.createModpack.value), ui.createDraft.value], [packTarget('a'), extras]);
  ui.openCreateModpack(packTarget('b'));
  assert.deepEqual([plain(ui.createModpack.value), ui.createDraft.value], [packTarget('b'), null]);
  ui.openCreateDraft(extras);
  assert.deepEqual([ui.createModpack.value, ui.createDraft.value], [null, extras]);
  ui.openCreate();
  assert.deepEqual([ui.createModpack.value, ui.createDraft.value, ui.createOpen.value], [null, null, true]);
});

/**
 * The real picks, install action and queue client; instance creation, the provider and navigation are stood in.
 * @param {{ unmade?: string[], refusing?: string[], whileQueueing?: (state: typeof import('../src/views/discover/state')) => void }} [given]
 * packs that are not created, instances that refuse a queue entry, and what the person does while one is asked for
 */
async function makeHarness({ unmade = [], refusing = [], whileQueueing } = {}) {
  /** @type {{ instance: string, picks: string[] }[]} */
  const installs = [];
  /** @type {unknown[]} */
  const navigations = [];
  const shared = {
    'views/discover/@preact/signals.ts': signals,
    'store.ts': { instances: signals.signal([]), config: signals.signal(null) },
    'ui-state.ts': {
      route: signals.signal({ name: 'discover' }),
      navigate: (/** @type {unknown} */ to) => navigations.push(to),
      openCreateDraft: () => {},
      openCreateModpack: () => {},
    },
  };
  const state = await loadSource('views/discover/state.ts', shared);
  const make = await loadSource('views/discover/make.ts', {
    ...shared,
    'views/discover/state.ts': { ...state },
    'api.ts': { api: async (/** @type {ApiCall} */ ...[, path, body]) => {
      assert.equal(path, '/content/install');
      const { instance_id: instance, selections } = /** @type {{ instance_id: string, selections: { canonical_id: string }[] }} */ (body);
      installs.push({ instance, picks: selections.map((entry) => entry.canonical_id) });
      whileQueueing?.(state);
      if (refusing.includes(instance)) throw new Error('Queue is full.');
      return queueResponse();
    } },
    'instance-create.ts': {
      createInstance: async (/** @type {import('../src/instance-create').CreateInstanceArgs} */ { modpack }) =>
        modpack && !unmade.includes(modpack.canonicalId)
          ? { ok: true, instance: { id: `instance of ${modpack.canonicalId}` } }
          : { ok: false, error: 'Not created.' },
    },
    'machines/downloads.ts': { applyInstallQueueResponse: async () => {}, reconcileUncertainMutation: async () => {} },
    'toast.ts': { toast: () => {} },
    'utils.ts': { errMessage: (/** @type {unknown} */ error) => (error instanceof Error ? error.message : String(error)) },
  });
  const picked = () => state.tray.value.map((item) => item.canonical_id);
  return { make, state, installs, navigations, picked };
}

/** @param {string} id @param {string[]} [drops] @returns {import('../src/views/discover/make').StagedPack} */
const stagedPack = (id, drops = []) => ({
  item: pick(id, 'modpack'),
  target: packTarget(id),
  fit: { loader: 'fabric', loaderLabel: 'Fabric', gameVersion: '1.21.1', drops: new Set(drops.map((drop) => `modrinth:${drop}`)) },
});

test('picks a made pack could not queue are handed back, and picks it leaves out are neither asked for nor held', async () => {
  const { make, installs } = await makeHarness({ refusing: ['refuses'] });
  const extras = [pick('shader', 'shader_pack'), pick('mod', 'mod')];
  const drops = new Set(['modrinth:mod']);
  assert.deepEqual(plain(await make.addPicksTo('takes', extras, drops)), []);
  assert.deepEqual(plain(await make.addPicksTo('refuses', extras, drops)), [extras[0]]);
  assert.deepEqual(plain(await make.addPicksTo('refuses', [extras[1]], drops)), []);
  assert.deepEqual(installs, [
    { instance: 'takes', picks: ['modrinth:shader'] },
    { instance: 'refuses', picks: ['modrinth:shader'] },
  ]);
});

test('several packs each take the picks that fit, and only what every instance took leaves the picks', async () => {
  const packs = [stagedPack('a'), stagedPack('b', ['mod'])];
  const extras = [pick('shader', 'shader_pack'), pick('mod', 'mod')];
  const staged = [...packs.map((pack) => pack.item), ...extras];

  const taken = await makeHarness();
  taken.state.tray.value = staged;
  await taken.make.createEach(packs, extras);
  assert.deepEqual(taken.installs, [
    { instance: 'instance of modrinth:a', picks: ['modrinth:shader', 'modrinth:mod'] },
    { instance: 'instance of modrinth:b', picks: ['modrinth:shader'] },
  ]);
  assert.deepEqual([taken.picked(), plain(taken.navigations)], [[], [{ name: 'instances' }]]);

  const refused = await makeHarness({ refusing: ['instance of modrinth:b'] });
  refused.state.tray.value = staged;
  await refused.make.createEach(packs, extras);
  assert.deepEqual([refused.picked(), refused.navigations], [['modrinth:shader'], []]);

  const switched = await makeHarness({
    whileQueueing: (state) => state.stageContent(extras[0], { id: 'newer', version_number: '2.0' }),
  });
  switched.state.tray.value = staged;
  await switched.make.createEach(packs, extras);
  assert.deepEqual(
    [plain(switched.state.tray.value), switched.navigations.length],
    [[{ ...extras[0], version_id: 'newer', version_label: '2.0' }], 1],
  );

  const unmade = await makeHarness({ unmade: ['modrinth:b'] });
  unmade.state.tray.value = staged;
  await unmade.make.createEach(packs, extras);
  assert.deepEqual(unmade.installs.map((install) => install.instance), ['instance of modrinth:a']);
  assert.deepEqual([unmade.picked(), unmade.navigations], [['modrinth:b', 'modrinth:shader', 'modrinth:mod'], []]);

  const unchecked = await makeHarness();
  unchecked.state.tray.value = staged;
  const unknown = { loader: 'fabric', loaderLabel: 'Fabric', gameVersion: '1.21.1', drops: null };
  await unchecked.make.createEach([packs[0], { ...packs[1], fit: unknown }], extras);
  assert.deepEqual(unchecked.installs.map((install) => install.instance), ['instance of modrinth:a']);
  assert.deepEqual([unchecked.picked(), unchecked.navigations], [['modrinth:b', 'modrinth:shader', 'modrinth:mod'], []]);
});

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

/** @param {Parameters<typeof import('../src/views/discover/install-workflow').createInstallWorkflow>[0]} actions */
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
  assert.deepEqual(plain(workflow.snapshot()), { busy: false, plan: null, installable: [], staged: [] });
});

test('install the rest re-plans without unavailable selections and what needs them', async () => {
  /** @type {import('../src/types-content').ContentSelection} */
  const ok = { canonical_id: 'modrinth:ok', kind: 'mod' };
  /** @type {import('../src/types-content').ContentSelection} */
  const gone = { canonical_id: 'modrinth:gone', kind: 'mod' };
  /** @type {import('../src/types-content').ContentSelection} */
  const needsGone = { canonical_id: 'modrinth:needs', kind: 'mod' };
  /** @param {string} id @param {string[]} requires @returns {import('../src/types-content').PlanItem} */
  const item = (id, requires) => ({
    canonical_id: id, title: id, kind: 'mod', project_id: id.slice('modrinth:'.length), version_id: 'v',
    version_number: '1', filename: `${id}.jar`, reason: 'selected', already_installed: false, update: false,
    dependencies: requires.map((project_id) => ({ project_id, kind: 'required' })),
  });
  /** @type {import('../src/types-content').ResolutionPlan} */
  const partialPlan = {
    loader: 'fabric', game_version: '1.21.1',
    items: [item(ok.canonical_id, []), item(needsGone.canonical_id, ['gone'])],
    conflicts: [{ canonical_id: gone.canonical_id, title: 'Gone', kind: 'unavailable', detail: 'No compatible version.' }],
    total_download_bytes: 0,
  };
  /** @type {unknown[]} */
  const planned = [];
  const workflow = await workflowHarness({
    addToInstance: async (_target, selections) => {
      planned.push(plain(selections));
      return planned.length === 1 ? { status: 'needs-confirmation', plan: partialPlan } : { status: 'queued' };
    },
    commitInstall: async () => ({ status: 'queued' }),
  });
  await workflow.add([ok, gone, needsGone], 'Three');
  assert.deepEqual(plain(workflow.snapshot().installable), [ok]);
  assert.equal(workflow.snapshot().staged.length, 3);
  assert.equal((await workflow.installRest()).status, 'queued');
  assert.deepEqual(planned, [[ok, gone, needsGone], [ok]]);
  assert.equal(workflow.snapshot().plan, null);
  assert.equal((await workflow.installRest()).status, 'failed');
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
  /** @type {import('../src/machines/discover-search').DiscoverSearchSnapshot} */
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
