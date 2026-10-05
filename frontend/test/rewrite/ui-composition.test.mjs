import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import vm from 'node:vm';
import test from 'node:test';

/**
 * @typedef {{
 *   'src/preferences/local.ts': typeof import('../../src/preferences/local'),
 *   'src/dto-contract.ts': typeof import('../../src/dto-contract'),
 *   'src/native.ts': typeof import('../../src/native'),
 *   'src/launch-notice-tracker.ts': typeof import('../../src/launch-notice-tracker'),
 *   'src/launch-response-adapters.ts': typeof import('../../src/launch-response-adapters'),
 *   'src/dto-install.ts': typeof import('../../src/dto-install'),
 *   'src/dto-core.ts': typeof import('../../src/dto-core'),
 *   'src/actions.ts': typeof import('../../src/actions'),
 *   'src/hooks/use-autosave.ts': typeof import('../../src/hooks/use-autosave'),
 *   'src/views/accounts/api.ts': typeof import('../../src/views/accounts/api'),
 *   'src/views/accounts/auth.ts': typeof import('../../src/views/accounts/auth'),
 *   'src/machines/accounts.ts': typeof import('../../src/machines/accounts'),
 *   'src/machines/accounts-state.ts': typeof import('../../src/machines/accounts-state'),
 *   'src/machines/downloads.ts': typeof import('../../src/machines/downloads'),
 *   'src/machines/download-view-models.ts': typeof import('../../src/machines/download-view-models'),
 *   'src/install-item.ts': typeof import('../../src/install-item'),
 *   'src/instance-readiness.ts': typeof import('../../src/instance-readiness'),
 *   'src/state.ts': typeof import('../../src/state'),
 *   'src/ui-state.ts': typeof import('../../src/ui-state'),
 *   'src/bootstrap.ts': typeof import('../../src/bootstrap'),
 *   'src/ui/DownloadFailureNotice.tsx': typeof import('../../src/ui/DownloadFailureNotice'),
 *   'src/shell/Topbar.tsx': typeof import('../../src/shell/Topbar'),
 *   'src/launch-presenters.ts': typeof import('../../src/launch-presenters'),
 * }} SourceModules
 * @typedef {import('../../src/types-settings').Config} Config
 * @typedef {Partial<Config> & { expected_revision: number, expected_account_selection_revision?: number }} ConfigPatch
 * @typedef {(method: string, path: string, patch?: ConfigPatch) => Promise<unknown>} ConfigApi
 * @typedef {[string, string, number | undefined]} RevisionCall
 */

const frontendPath = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const frontendRoot = pathToFileURL(`${frontendPath}/`);
let dependencyRequire = createRequire(new URL('package.json', frontendRoot));
try {
  dependencyRequire.resolve('typescript');
} catch {
  dependencyRequire = createRequire(new URL('../legacy/frontend/package.json', frontendRoot));
}
const ts = /** @type {typeof import('typescript')} */ (dependencyRequire('typescript'));
const jsxRuntime = /** @type {typeof import('preact/jsx-runtime')} */ (dependencyRequire('preact/jsx-runtime'));
const signals = /** @type {typeof import('@preact/signals')} */ (dependencyRequire('@preact/signals'));
/** @template T @param {T} value */
const signal = (value) => signals.signal(value);

/**
 * @template {keyof SourceModules} T
 * @param {T} path
 * @param {Record<string, unknown>} [imports]
 * @param {Record<string, unknown>} [globals]
 * @returns {SourceModules[T]}
 */
function loadSource(path, imports = {}, globals = {}) {
  const filename = fileURLToPath(new URL(path, frontendRoot));
  const { outputText } = ts.transpileModule(readFileSync(filename, 'utf8'), {
    fileName: filename,
    compilerOptions: {
      module: ts.ModuleKind.CommonJS,
      target: ts.ScriptTarget.ES2022,
      jsx: ts.JsxEmit.ReactJSX,
      jsxImportSource: 'preact',
    },
  });
  const exports = {};
  vm.runInNewContext(
    outputText,
    {
      exports,
      /** @param {string} id */
      require(id) {
        if (id === 'preact/jsx-runtime') return jsxRuntime;
        if (id === '@preact/signals') return signals;
        if (Object.prototype.hasOwnProperty.call(imports, id)) return imports[id];
        throw new Error(`Unreviewed UI composition dependency: ${id}`);
      },
      Date,
      Error,
      setTimeout,
      clearTimeout,
      console: { error() {} },
      ...globals,
    },
    { filename },
  );
  return /** @type {SourceModules[T]} */ (exports);
}

/** @param {Array<[string, string]>} [entries] */
function storage(entries = []) {
  const values = new Map(entries);
  return {
    /** @param {string} key */
    getItem(key) {
      return values.get(key) ?? null;
    },
    /** @param {string} key @param {string} value */
    setItem(key, value) {
      values.set(key, value);
    },
    values,
  };
}

/** @param {Pick<Storage, 'getItem'>} localStorage @param {string} key */
function storedValue(localStorage, key) {
  const value = localStorage.getItem(key);
  assert.ok(value !== null, `Missing stored value: ${key}`);
  return value;
}

const preferences = loadSource('src/preferences/local.ts');
const browserPreferenceImports = {
  './preferences/local': preferences,
  './native': { hasNativeDesktopRuntime: () => false },
  './preferences/persistence': { canEditPreferences: () => true },
};
const contract = loadSource('src/dto-contract.ts');
const notices = loadSource('src/launch-notice-tracker.ts');
const sessions = loadSource('src/launch-response-adapters.ts', {
  './dto-contract': contract,
  './launch-notice-tracker': notices,
});
const installs = loadSource('src/dto-install.ts', { './dto-contract': contract });
const core = loadSource('src/dto-core.ts', { './dto-contract': contract, './dto-install': installs });

/** @param {Partial<Config>} [patch] @returns {Config} */
function configSnapshot(patch = {}) {
  return {
    revision: 7,
    account_selection_revision: 4,
    username: 'Player',
    launch_auth_mode: 'offline',
    max_memory_mb: 4096,
    min_memory_mb: 512,
    java_path_override: '',
    window_width: 0,
    window_height: 0,
    jvm_preset: '',
    performance_mode: 'managed',
    theme: '',
    custom_hue: null,
    custom_vibrancy: null,
    lightness: null,
    onboarding_done: false,
    telemetry_enabled: false,
    discord_rpc_enabled: true,
    discord_rpc_onboarding_seen: false,
    music_enabled: null,
    music_volume: null,
    music_track: 0,
    ...patch,
  };
}

test('replacement config accepts the real Guardian-free settings projection and exact revisions', () => {
  const parsed = core.configResponse(configSnapshot());
  assert.equal(JSON.stringify(parsed), JSON.stringify(configSnapshot()));
  assert.equal('guardian_mode' in parsed, false);
  for (const revision of [-1, 0.5, Number.MAX_SAFE_INTEGER + 1]) {
    assert.throws(() => core.configResponse(configSnapshot({ revision })), /revision/);
    assert.throws(
      () => core.configResponse(configSnapshot({ account_selection_revision: revision })),
      /selection revision/,
    );
  }
  assert.throws(
    () => core.configResponse(configSnapshot({ account_selection_revision: undefined })),
    /selection revision/,
  );
});

/** @param {ConfigApi} api @param {Config} [initial] */
function configWriterHarness(api, initial = configSnapshot()) {
  const config = signal(initial);
  const actions = loadSource('src/actions.ts', {
    './store': { config },
    './launch-response-adapters': sessions,
  });
  const writer = loadSource('src/hooks/use-autosave.ts', {
    'preact/hooks': {},
    '../actions': actions,
    '../api': { api },
    '../dto-core': core,
    '../store': { config },
    '../toast': { toast() {} },
    '../utils': { errMessage: /** @param {Error} error */ (error) => error.message },
  });
  return { ...writer, config, setConfig: actions.setConfig };
}

function gate() {
  let resolve = () => {};
  const promise = /** @type {Promise<void>} */ (
    new Promise((done) => {
      resolve = done;
    })
  );
  return { promise, resolve };
}

test('simultaneous settings edits preserve both fields and use successive committed revisions', async () => {
  let backend = configSnapshot();
  /** @type {RevisionCall[]} */
  const calls = [];
  const writer = configWriterHarness(async (method, path, patch) => {
    assert.ok(patch);
    calls.push([method, path, patch.expected_revision]);
    assert.equal(patch.expected_revision, backend.revision);
    assert.equal('expected_account_selection_revision' in patch, false);
    const { expected_revision, ...fields } = patch;
    backend = { ...backend, ...fields, revision: expected_revision + 1 };
    return backend;
  });
  await Promise.all([
    writer.saveConfigPatch({ performance_mode: 'vanilla' }),
    writer.saveConfigPatch({ telemetry_enabled: true }),
  ]);
  assert.deepEqual(calls, [
    ['PUT', '/config', 7],
    ['PUT', '/config', 8],
  ]);
  assert.equal(writer.config.value.performance_mode, 'vanilla');
  assert.equal(writer.config.value.telemetry_enabled, true);
});

test('an uncertain config response is reconciled with a read before the next independent save', async () => {
  let backend = configSnapshot();
  /** @type {RevisionCall[]} */
  const calls = [];
  const writer = configWriterHarness(async (method, path, patch) => {
    calls.push([method, path, patch?.expected_revision]);
    if (method === 'GET') return backend;
    assert.ok(patch);
    assert.equal(patch.expected_revision, backend.revision);
    const { expected_revision, ...fields } = patch;
    backend = { ...backend, ...fields, revision: expected_revision + 1 };
    if (expected_revision === 7) throw new Error('Reply lost after save');
    return backend;
  });
  await assert.rejects(writer.saveConfigPatch({ theme: 'end' }), /Reply lost/);
  await writer.saveConfigPatch({ music_volume: 15 });
  assert.deepEqual(calls, [
    ['PUT', '/config', 7],
    ['GET', '/config', undefined],
    ['PUT', '/config', 8],
  ]);
  assert.equal(writer.config.value.theme, 'end');
  assert.equal(writer.config.value.music_volume, 15);
});

test('a delayed successful save acknowledges its commit without replacing newer settings or selection', async () => {
  for (const newer of [
    configSnapshot({ revision: 9, theme: 'end' }),
    configSnapshot({ revision: 8, account_selection_revision: 5, username: 'Different' }),
  ]) {
    const entered = gate();
    const release = gate();
    const acknowledgement = configSnapshot({ revision: 8, performance_mode: 'vanilla' });
    const writer = configWriterHarness(async (method, path, patch) => {
      assert.equal(method, 'PUT');
      assert.equal(path, '/config');
      assert.equal(patch?.expected_revision, 7);
      entered.resolve();
      await release.promise;
      return acknowledgement;
    });
    const pending = writer.saveConfigPatch({ performance_mode: 'vanilla' });
    await entered.promise;
    assert.equal(writer.setConfig(newer), true);
    release.resolve();
    assert.equal(JSON.stringify(await pending), JSON.stringify(acknowledgement));
    assert.equal(writer.config.value, newer);
  }
});

test('a delayed failed-save reconciliation cannot replace newer settings or selection and never replays the write', async () => {
  for (const newer of [
    configSnapshot({ revision: 9, theme: 'end' }),
    configSnapshot({ revision: 8, account_selection_revision: 5, username: 'Different' }),
  ]) {
    const entered = gate();
    const release = gate();
    /** @type {string[]} */
    const calls = [];
    const writer = configWriterHarness(async (method) => {
      calls.push(method);
      if (method === 'PUT') throw new Error('Reply lost after save');
      entered.resolve();
      await release.promise;
      return configSnapshot({ revision: 8, performance_mode: 'vanilla' });
    });
    const pending = writer.saveConfigPatch({ performance_mode: 'vanilla' });
    await entered.promise;
    assert.equal(writer.setConfig(newer), true);
    release.resolve();
    await assert.rejects(pending, /Reply lost after save/);
    assert.equal(writer.config.value, newer);
    assert.deepEqual(calls, ['PUT', 'GET']);
  }
});

test('identity config edits carry the selected account revision alongside the settings revision', async () => {
  /** @type {Partial<Config>[]} */
  const patches = [{ username: 'Renamed' }, { launch_auth_mode: 'online' }];
  for (const patch of patches) {
    const writer = configWriterHarness(async (method, path, body) => {
      assert.equal(method, 'PUT');
      assert.equal(path, '/config');
      assert.ok(body);
      assert.equal(body.expected_revision, 7);
      assert.equal(body.expected_account_selection_revision, 4);
      return configSnapshot({ ...patch, revision: 8, account_selection_revision: 5 });
    });
    await writer.saveConfigPatch(patch);
    assert.equal(writer.config.value.account_selection_revision, 5);
  }
});

test('a queued identity edit cannot adopt an account selected after the edit was committed', async () => {
  /** @type {RevisionCall[]} */
  const calls = [];
  const writer = configWriterHarness(async (method, path, patch) => {
    calls.push([method, path, patch?.expected_account_selection_revision]);
    if (method === 'GET') return configSnapshot({ username: 'Different', account_selection_revision: 5 });
    assert.ok(patch);
    assert.equal(patch.expected_account_selection_revision, 4);
    throw new Error('Account selection changed');
  });
  const pending = writer.saveConfigPatch({ username: 'RenameOriginal' });
  writer.config.value = configSnapshot({ username: 'Different', account_selection_revision: 5 });
  await assert.rejects(pending, /Account selection changed/);
  assert.deepEqual(calls, [
    ['PUT', '/config', 4],
    ['GET', '/config', undefined],
  ]);
  assert.equal(writer.config.value.username, 'Different');
});

/** @param {{ invalidReply?: boolean, lostReply?: boolean, nativeInvoke?: (command: string) => Promise<unknown> }} [options] */
function accountRenameHarness({ invalidReply = false, lostReply = false, nativeInvoke } = {}) {
  const disabled = { state_id: 'offline', label: 'Offline', enabled: false };
  let account = {
    account_id: 'offline-before',
    account_revision: 2,
    profile_revision: 0,
    credential_revision: 0,
    kind: 'offline',
    display_name: 'Player',
    active: true,
    msa_authenticated: false,
    msa_refresh_available: false,
    minecraft_profile_ready: false,
    minecraft_ownership_verified: false,
    online_action: disabled,
    refresh_action: disabled,
    profile_sync_action: disabled,
    view_model: { detail: 'Offline identity' },
  };
  let selectionRevision = 4;
  /** @type {Array<[string, string, unknown]>} */
  const calls = [];
  /** @type {Array<Parameters<typeof import('../../src/toast').toast>>} */
  const toasts = [];
  /** @type {string[]} */
  const errors = [];
  const utils = {
    showError: /** @param {string} message */ (message) => {
      errors.push(message);
    },
  };
  /** @param {string} id @param {boolean} launchable @returns {import('../../src/types-instance').EnrichedInstance} */
  function instance(id, launchable) {
    return {
      id,
      name: id,
      version_id: '1.21',
      created_at: '2026-09-08T08:00:00Z',
      version_display: {
        loader_key: 'vanilla',
        loader_label: 'Vanilla',
        minecraft_label: '1.21',
        loader_version_label: '',
        loader_detail_label: '',
        summary_label: '1.21',
        supports_mods: false,
      },
      launchable,
      launch_action: {
        state_id: launchable ? 'ready' : 'blocked',
        label: launchable ? 'Launch' : 'Unavailable',
        tone: launchable ? 'ok' : 'warn',
        launchable,
        primary_action: launchable ? 'launch' : 'blocked',
      },
      saves_count: 0,
      mods_count: 0,
      resource_count: 0,
      shader_count: 0,
    };
  }
  const store = {
    config: signal(configSnapshot()),
    instances: signal([instance('first', false), instance('second', false)]),
    launchSessions: signal({}),
  };
  /** @type {(path: string, response: unknown) => Promise<unknown>} */
  let read = async (_path, response) => response;
  /**
   * @param {string} method
   * @param {string} path
   * @param {{expected_account_revision: number, expected_selection_revision: number, username: string}} [patch]
   */
  const api = async (method, path, patch) => {
    calls.push([method, path, patch]);
    if (method === 'PATCH') {
      assert.ok(patch);
      assert.equal(patch.expected_account_revision, 2);
      assert.equal(patch.expected_selection_revision, 4);
      account = { ...account, account_id: 'offline-after', display_name: patch.username, account_revision: 3 };
      selectionRevision++;
      if (lostReply) throw new Error('Account response lost');
      return { status: 'account_updated', account: invalidReply ? undefined : account };
    }
    if (path === '/config')
      return read(
        path,
        configSnapshot({ username: account.display_name, account_selection_revision: selectionRevision }),
      );
    if (path === '/instances')
      return read(path, { instances: [instance('first', true), instance('second', true)], last_instance_id: 'first' });
    if (path === '/instances/first') return read(path, instance('first', true));
    if (path === '/accounts')
      return read(path, {
        revision: selectionRevision,
        selection_revision: selectionRevision,
        launch_auth_mode: 'offline',
        active_account_id: account.account_id,
        accounts: [account],
      });
    if (path === '/auth/status')
      return read(path, {
        selection_revision: selectionRevision,
        launch_auth_mode: 'offline',
        mode: 'offline',
        username: account.display_name,
        uuid: 'offline',
        provider: 'offline',
        verified: false,
        skin_source: 'default',
        login_available: !!nativeInvoke,
        login_reason: nativeInvoke ? '' : 'Desktop required',
        msa_authenticated: false,
        msa_refresh_available: false,
        minecraft_profile_ready: false,
        minecraft_ownership_verified: false,
        online_action: disabled,
        refresh_action: disabled,
        profile_sync_action: disabled,
        skin_action: disabled,
      });
    throw new Error(`Unexpected account request ${method} ${path}`);
  };
  const accountsApi = loadSource('src/views/accounts/api.ts', {
    '../../api': { api, isApiError: () => false },
    '../../default-skins': { DEFAULT_SKINS: [] },
  });
  const auth = loadSource('src/views/accounts/auth.ts', { './api': accountsApi });
  const accountState = loadSource('src/machines/accounts-state.ts');
  const readiness = loadSource(
    'src/instance-readiness.ts',
    {
      './api': { api },
      './dto-core': core,
      './store': store,
      './utils': utils,
    },
    { window: { setTimeout } },
  );
  const machine = loadSource(
    'src/machines/accounts.ts',
    {
      './accounts-state': accountState,
      '../store': store,
      '../actions': loadSource('src/actions.ts', { './store': store, './launch-response-adapters': sessions }),
      '../api': { api, isApiError: () => false },
      '../native': nativeInvoke
        ? loadSource(
            'src/native.ts',
            { './dto-contract': contract },
            { window: { __TAURI__: { core: { invoke: nativeInvoke } } } },
          )
        : {},
      '../player-name': { promptPlayerName: async () => 'Renamed' },
      '../player-skin': { refreshAccountSkin() {} },
      '../toast': {
        toast: /** @param {Parameters<typeof import('../../src/toast').toast>} args */ (...args) => toasts.push(args),
      },
      '../ui/Dialog': {},
      '../views/accounts/api': accountsApi,
      '../views/accounts/auth': auth,
      '../dto-core': core,
      '../instance-readiness': readiness,
      '../utils': utils,
    },
    { console: { warn() {} } },
  );
  assert.equal(machine.accountsSnapshot, accountState.accountsSnapshot);
  return {
    machine,
    calls,
    toasts,
    errors,
    store,
    readiness,
    instance,
    /** @param {typeof read} next */
    read(next) {
      read = next;
    },
  };
}

/** @param {SourceModules['src/machines/accounts.ts']} machine */
function activeAccount(machine) {
  const account = machine.activeAccount();
  assert.ok(account);
  return account;
}

test('offline stage rename uses account fences and accepts the backend-generated replacement identity', async () => {
  const { machine, calls, toasts } = accountRenameHarness();
  await machine.refreshAccountsData();
  const saved = await machine.saveOfflineIdentityName(activeAccount(machine), 'Renamed');
  assert.equal(saved, true);
  assert.equal(activeAccount(machine).account_id, 'offline-after');
  assert.equal(activeAccount(machine).display_name, 'Renamed');
  const mutations = calls.filter(([method]) => method !== 'GET');
  assert.equal(mutations.length, 1);
  assert.equal(mutations[0][1], '/accounts/offline-before');
  assert.equal(toasts.length, 1);
});

test('switcher rename verifies the returned replacement identity after its prompt', async () => {
  const { machine, toasts } = accountRenameHarness();
  await machine.refreshAccountsData();
  await machine.renameOfflineIdentity(activeAccount(machine));
  assert.equal(machine.accountsNotice.value, null);
  assert.equal(activeAccount(machine).account_id, 'offline-after');
  assert.equal(toasts.length, 1);
});

test('an account change refreshes backend launch availability for every retained instance', async () => {
  const { machine, calls, store } = accountRenameHarness();
  await machine.refreshAccountsData();
  assert.ok(store.instances.value.every((instance) => !instance.launch_action.launchable));
  assert.equal(await machine.saveOfflineIdentityName(activeAccount(machine), 'Renamed'), true);
  assert.deepEqual(
    store.instances.value.map((instance) => [instance.id, instance.launch_action.label]),
    [
      ['first', 'Launch'],
      ['second', 'Launch'],
    ],
  );
  assert.equal(calls.filter(([, path]) => path === '/instances').length, 1);
  assert.equal(calls[calls.length - 1]?.[1], '/instances');
});

test('acknowledged account changes settle with a permanent instance refusal', async () => {
  const h = accountRenameHarness();
  await h.machine.refreshAccountsData();
  h.store.instances.value = Array.from({ length: 7 }, (_, index) => h.instance(`retained-${index}`, false));
  h.read(async (path, response) =>
    path === '/instances'
      ? {
          instances: h.store.instances.value.map((row, index) => h.instance(row.id, index !== 3)),
          last_instance_id: null,
        }
      : response,
  );

  assert.equal(await h.machine.saveOfflineIdentityName(activeAccount(h.machine), 'Renamed'), true);
  assert.equal(h.machine.accountsOp.value, null);
  assert.equal(h.machine.accountsNotice.value, null);
  assert.equal(h.toasts.length, 1);
  assert.deepEqual(
    h.store.instances.value.map((row) => row.launch_action.launchable),
    [true, true, true, false, true, true, true],
  );
  assert.equal(h.calls.filter(([, path]) => path === '/instances').length, 1);
});

test('an acknowledged account change stays successful when readiness is unavailable', async () => {
  const h = accountRenameHarness();
  await h.machine.refreshAccountsData();
  const previous = h.store.instances.value;
  h.read(async (path, response) => {
    if (path === '/instances') throw new Error('private transport detail');
    return response;
  });
  assert.equal(await h.machine.saveOfflineIdentityName(activeAccount(h.machine), 'Renamed'), true);
  assert.equal(activeAccount(h.machine).display_name, 'Renamed');
  assert.equal(h.machine.accountsNotice.value, null);
  assert.equal(h.machine.accountsOp.value, null);
  assert.equal(h.store.instances.value, previous);
  assert.equal(h.toasts.length, 1);
  assert.deepEqual(h.errors, ['Could not refresh launch availability. Refresh the launcher to check again.']);
  assert.equal(h.calls.filter(([, path]) => path === '/instances').length, 1);
  assert.equal(h.calls.filter(([, path]) => path === '/config').length, 1);
  assert.equal(h.calls.filter(([method]) => method === 'PATCH').length, 1);
});

test('invalid rename confirmation reconciles accounts without publishing success or replaying mutation', async () => {
  const { machine, calls, toasts } = accountRenameHarness({ invalidReply: true });
  await machine.refreshAccountsData();
  assert.equal(await machine.saveOfflineIdentityName(activeAccount(machine), 'Renamed'), false);
  assert.equal(activeAccount(machine).account_id, 'offline-after');
  assert.ok(machine.accountsNotice.value);
  assert.match(machine.accountsNotice.value, /did not return the renamed offline identity/);
  assert.equal(calls.filter(([method]) => method !== 'GET').length, 1);
  assert.equal(toasts.length, 0);
});

test('a lost committed account reply still refreshes readiness without replay or success', async () => {
  const { machine, calls, toasts } = accountRenameHarness({ lostReply: true });
  await machine.refreshAccountsData();
  assert.equal(await machine.saveOfflineIdentityName(activeAccount(machine), 'Renamed'), false);
  assert.equal(activeAccount(machine).account_id, 'offline-after');
  assert.equal(calls.filter(([method]) => method === 'PATCH').length, 1);
  assert.equal(calls.filter(([, path]) => path === '/instances').length, 1);
  assert.match(machine.accountsNotice.value ?? '', /Account response lost/);
  assert.deepEqual(toasts, []);
});

test('a lost account reply retains bounded reconciliation for unavailable instances', async () => {
  const h = accountRenameHarness({ lostReply: true });
  await h.machine.refreshAccountsData();
  h.read(async (path, response) =>
    path === '/instances'
      ? { instances: [h.instance('first', true), h.instance('second', false)], last_instance_id: null }
      : response,
  );
  assert.equal(await h.machine.saveOfflineIdentityName(activeAccount(h.machine), 'Renamed'), false);
  assert.equal(h.calls.filter(([method]) => method === 'PATCH').length, 1);
  assert.equal(h.calls.filter(([, path]) => path === '/instances').length, 2);
  assert.equal(h.store.instances.value[1].launch_action.launchable, false);
  assert.match(h.machine.accountsNotice.value ?? '', /Account response lost/);
  assert.deepEqual(h.toasts, []);
});

for (const accountless of [false, true]) {
  test(`unchanged ${accountless ? 'accountless' : 'selected-account'} cancellation avoids rechecking six retained instances`, async () => {
    const h = accountRenameHarness({ nativeInvoke: async () => ({ status: 'cancelled' }) });
    const retained = Array.from({ length: 6 }, (_, index) => h.instance(`retained-${index}`, index !== 3));
    h.store.instances.value = retained;
    h.read(async (path, response) => {
      if (path === '/instances') return { instances: retained, last_instance_id: null };
      if (accountless && path === '/accounts')
        return { ...contract.dtoRecord(response, 'Accounts'), active_account_id: null, accounts: [] };
      if (accountless && path === '/auth/status')
        return { ...contract.dtoRecord(response, 'Status'), username: '', uuid: '' };
      return response;
    });
    await h.machine.refreshAccountsData();
    const config = h.store.config.value;
    h.calls.length = 0;
    for (let attempt = 0; attempt < 2; attempt++) {
      assert.equal(await h.machine.signInWithMicrosoftAccount(), null);
      assert.equal(h.machine.accountsOp.value, null);
      assert.equal(h.machine.accountsNotice.value, null);
    }
    assert.deepEqual(
      h.calls.map(([, path]) => path),
      ['/config', '/accounts', '/auth/status', '/config', '/accounts', '/auth/status'],
    );
    assert.equal(h.store.config.value, config);
    assert.equal(h.store.instances.value, retained);
    assert.equal(h.store.instances.value[3].launch_action.launchable, false);
    assert.deepEqual(h.toasts, []);
  });
}

for (const change of [
  'selection',
  'selection-aba',
  'settings',
  'directory-action',
  'status-action',
  'config-unavailable',
  'accounts-unavailable',
  'incoherent-selection',
  'missing-directory-action',
  'missing-status-action',
]) {
  test(`sign-in cancellation still refreshes readiness after ${change}`, async () => {
    const h = accountRenameHarness({ nativeInvoke: async () => ({ status: 'cancelled' }) });
    const actionChange = change === 'directory-action' || change === 'status-action';
    let changed = false;
    h.read(async (path, response) => {
      const value = contract.dtoRecord(response, 'Account fixture');
      if (
        changed &&
        ((change === 'config-unavailable' && path === '/config') ||
          (change === 'accounts-unavailable' && path === '/accounts'))
      )
        throw new Error('Read unavailable');
      const selection =
        changed && (change === 'selection' || change === 'selection-aba') ? (change === 'selection-aba' ? 6 : 5) : 4;
      const online = { state_id: 'online_ready', label: 'Online ready', enabled: true };
      const expired = { state_id: 'online_sign_in_required', label: 'Sign in required', enabled: false };
      const profile = { id: '12345678123442348234123456789abc', name: 'Player', skins: [], capes: [] };
      if (path === '/config')
        return {
          ...value,
          launch_auth_mode: actionChange ? 'online' : 'offline',
          revision: changed && change === 'settings' ? 8 : 7,
          account_selection_revision: selection,
        };
      if (path === '/accounts') {
        assert.ok(Array.isArray(value.accounts));
        return {
          ...value,
          revision: selection,
          selection_revision: selection,
          launch_auth_mode: actionChange ? 'online' : 'offline',
          accounts: value.accounts.map((account) =>
            changed && change === 'missing-directory-action'
              ? { ...account, online_action: undefined }
              : actionChange
                ? {
                    ...account,
                    kind: 'microsoft',
                    login_id: 'current-login',
                    minecraft_profile: profile,
                    minecraft_profile_ready: true,
                    minecraft_ownership_verified: true,
                    online_action: changed && change === 'directory-action' ? expired : online,
                  }
                : account,
          ),
        };
      }
      if (path === '/auth/status')
        return {
          ...value,
          selection_revision: changed && change === 'incoherent-selection' ? 5 : selection,
          launch_auth_mode: actionChange ? 'online' : 'offline',
          ...(changed && change === 'missing-status-action' ? { online_action: undefined } : {}),
          ...(actionChange
            ? {
                minecraft_profile: profile,
                minecraft_profile_ready: true,
                minecraft_ownership_verified: true,
                online_action: changed && change === 'status-action' ? expired : online,
              }
            : {}),
        };
      return response;
    });
    if (actionChange) h.store.config.value = configSnapshot({ launch_auth_mode: 'online' });
    await h.machine.refreshAccountsData();
    const pending = h.machine.signInWithMicrosoftAccount();
    changed = true;
    assert.equal(await pending, null);
    assert.equal(h.calls.filter(([, path]) => path === '/instances').length, 1);
    assert.equal(h.machine.accountsOp.value, null);
    assert.deepEqual(h.toasts, []);
    if (change === 'selection-aba') {
      assert.equal(activeAccount(h.machine).account_id, 'offline-before');
      assert.equal(h.machine.accountsSnapshot.value.selection_revision, 6);
    }
    if (change.startsWith('missing-')) assert.equal(h.machine.accountsSnapshot.value.state, 'unavailable');
  });
}

test('unchanged sign-in cancellation preserves an in-flight readiness owner and its config fence', async () => {
  const h = accountRenameHarness({ nativeInvoke: async () => ({ status: 'cancelled' }) });
  await h.machine.refreshAccountsData();
  /** @type {(value: unknown) => void} */
  let reply = () => assert.fail('Readiness request was not started');
  const response = new Promise((resolve) => {
    reply = resolve;
  });
  h.read(async (path, value) => (path === '/instances/first' ? response : value));
  const config = h.store.config.value;
  const read = h.readiness.refreshInstanceReadiness('first');
  await h.machine.signInWithMicrosoftAccount();
  assert.equal(h.store.instances.value[0].launch_action.launchable, false);
  reply(h.instance('first', true));
  await read;
  assert.equal(h.store.instances.value[0].launch_action.launchable, true);
  assert.equal(h.store.config.value, config);
  assert.equal(h.calls.filter(([, path]) => path === '/instances').length, 0);
});

test('native Microsoft sign-in preserves audited refusal causes through the accounts caller', async () => {
  for (const message of [
    'Microsoft sign-in timed out.',
    'Microsoft sign-in services are unavailable (HTTP 503)',
    'Secure credential storage is unavailable.',
  ]) {
    /** @type {string[]} */
    const commands = [];
    const { machine, calls, toasts } = accountRenameHarness({
      nativeInvoke: async (command) => {
        commands.push(command);
        throw message;
      },
    });
    await machine.refreshAccountsData();
    assert.equal(machine.microsoftSignInAvailable(), true);
    const before = JSON.stringify(machine.accountsSnapshot.value);
    assert.equal(await machine.signInWithMicrosoftAccount(), null);
    assert.deepEqual(commands, ['microsoft_sign_in']);
    assert.equal(machine.accountsNotice.value, message);
    assert.equal(machine.accountsOp.value, null);
    assert.equal(JSON.stringify(machine.accountsSnapshot.value), before);
    assert.ok(calls.every(([method]) => method === 'GET'));
    assert.equal(calls.filter(([, path]) => path === '/instances').length, 1);
    assert.deepEqual(toasts, []);
  }
});

test('unexpected native Microsoft rejection shapes cannot expose transport details', async () => {
  for (const reason of [
    new Error('private transport detail'),
    '',
    '  ',
    null,
    {
      get message() {
        throw new Error('unexpected message access');
      },
      toString() {
        throw new Error('unexpected coercion');
      },
    },
  ]) {
    let invocations = 0;
    const { machine, calls, toasts } = accountRenameHarness({
      nativeInvoke: async () => {
        invocations++;
        throw reason;
      },
    });
    await machine.refreshAccountsData();
    assert.equal(machine.microsoftSignInAvailable(), true);
    const before = JSON.stringify(machine.accountsSnapshot.value);
    assert.equal(await machine.signInWithMicrosoftAccount(), null);
    assert.equal(invocations, 1);
    assert.equal(machine.accountsNotice.value, 'Microsoft sign-in could not be completed.');
    assert.equal(machine.accountsOp.value, null);
    assert.equal(JSON.stringify(machine.accountsSnapshot.value), before);
    assert.ok(calls.every(([method]) => method === 'GET'));
    assert.equal(calls.filter(([, path]) => path === '/instances').length, 1);
    assert.deepEqual(toasts, []);
  }
});

test('native sign-in cancellation releases busy for retry without weakening DTO or selection checks', async () => {
  /** @type {unknown} */
  let response = { status: 'cancelled', login_id: null, profile_name: null, owns_minecraft_java: null };
  let invocations = 0;
  const { machine, calls, toasts } = accountRenameHarness({
    nativeInvoke: async (command) => {
      assert.equal(command, 'microsoft_sign_in');
      invocations++;
      return response;
    },
  });
  await machine.refreshAccountsData();
  assert.equal(machine.microsoftSignInAvailable(), true);
  const before = JSON.stringify(machine.accountsSnapshot.value);
  for (let attempt = 0; attempt < 2; attempt++) {
    assert.equal(await machine.signInWithMicrosoftAccount(), null);
    assert.equal(machine.accountsNotice.value, null);
    assert.equal(machine.accountsOp.value, null);
  }
  assert.equal(invocations, 2);
  assert.equal(calls.filter(([, path]) => path === '/instances').length, 0);
  response = { status: 'unexpected' };
  assert.equal(await machine.signInWithMicrosoftAccount(), null);
  assert.equal(machine.accountsNotice.value, 'Native Microsoft sign-in response was invalid.');
  assert.equal(calls.filter(([, path]) => path === '/instances').length, 1);
  response = {
    status: 'authenticated',
    login_id: 'different-login',
    profile_name: 'Different',
    owns_minecraft_java: true,
  };
  assert.equal(await machine.signInWithMicrosoftAccount(), null);
  assert.equal(machine.accountsNotice.value, 'Microsoft sign-in completed, but account state is unavailable.');
  assert.equal(machine.accountsOp.value, null);
  assert.equal(invocations, 4);
  assert.equal(calls.filter(([, path]) => path === '/instances').length, 2);
  assert.equal(JSON.stringify(machine.accountsSnapshot.value), before);
  assert.ok(calls.every(([method]) => method === 'GET'));
  assert.deepEqual(toasts, []);
});

test('browser preferences persist edits and return independent preference maps', () => {
  const localStorage = storage();
  const state = loadSource('src/state.ts', browserPreferenceImports, { localStorage });
  assert.equal(state.local.theme, 'obsidian');
  state.local.theme = 'birch';
  state.saveLocalState();
  assert.equal(JSON.parse(storedValue(localStorage, state.STORAGE_KEY)).theme, 'birch');
  const first = state.loadLocalState();
  first.selectedSkinsByAccount.a = 'skin';
  assert.equal(state.loadLocalState().selectedSkinsByAccount.a, undefined);
});

test('stored browser preferences validate values and recover with fresh defaults', () => {
  const localStorage = storage([['axial_rewrite_ui', JSON.stringify({ theme: 'end', sounds: false })]]);
  const state = loadSource('src/state.ts', browserPreferenceImports, { localStorage });
  assert.equal(state.local.theme, 'end');
  assert.equal(state.local.sounds, false);
  localStorage.setItem(state.STORAGE_KEY, JSON.stringify({ theme: 'unknown', customHue: 'red' }));
  assert.equal(state.loadLocalState().theme, 'obsidian');
  const first = state.loadLocalState();
  first.shortcuts.play = { key: 'p' };
  assert.equal(state.loadLocalState().shortcuts.play, undefined);
});

test('route restoration reads saved routes and rejects incomplete route identities', () => {
  const localStorage = storage();
  const ui = loadSource('src/ui-state.ts', browserPreferenceImports, { localStorage });
  ui.restoreRoute();
  const initialRoute = ui.route.value;
  assert.equal(initialRoute.name, 'home');
  localStorage.setItem(ui.ROUTE_STORAGE_KEY, JSON.stringify({ name: 'content', id: 'pack', target: 'new' }));
  ui.restoreRoute();
  const restoredRoute = ui.route.value;
  assert.ok(restoredRoute.name === 'content');
  assert.equal(restoredRoute.target, 'new');
  localStorage.setItem(ui.ROUTE_STORAGE_KEY, JSON.stringify({ name: 'instance', id: '' }));
  ui.restoreRoute();
  assert.equal(ui.route.value.name, 'content');
});

const progress = { phase_id: 'starting', label: 'Preparing', progress_pct: 0, terminal: false, failed: false };
const action = { action: 'retry', label: 'Retry install', enabled: false, disabled_reason: 'Busy' };
const failure = {
  state_id: 'failed',
  title: 'Install failed',
  tone: 'err',
  summary: 'Provider rejected download',
  details: ['Retry when available'],
  retry_action: action,
  dismiss_action: { ...action, action: 'dismiss' },
};
function queue() {
  return {
    queue_epoch: 'queue-1',
    revision: 7,
    registry_revision: 1,
    items: [],
    view_model: {
      state_id: 'idle',
      status_label: 'Idle',
      title: 'Downloads',
      summary: '',
      queued_count: 0,
      queued_count_label: '0',
      queued_item_label: 'items',
      section_title: 'Queue',
      empty_title: 'No downloads',
      empty_summary: '',
    },
    latest_failure: {
      failed_at_ms: 1788854400000,
      queue_id: 'q1',
      install_id: 'i1',
      operation_id: 'o1',
      label: 'Minecraft',
      install_item: { version_id: '1.21.1' },
      failure_view_model: failure,
    },
  };
}

test('queue decoding preserves producer revision, failure identity, timestamp and action authority', () => {
  const parsed = installs.installQueueStateResponse(queue());
  assert.equal(parsed.revision, 7);
  assert.ok(parsed.latest_failure);
  assert.equal(parsed.latest_failure.queue_id, 'q1');
  assert.equal(parsed.latest_failure.failed_at_ms, 1788854400000);
  assert.equal(parsed.latest_failure.failure_view_model.retry_action.enabled, false);
  assert.equal(parsed.latest_failure.failure_view_model.retry_action.disabled_reason, 'Busy');
  for (const revision of [undefined, -1, 0.5, Number.MAX_SAFE_INTEGER + 1]) {
    assert.throws(() => installs.installQueueStateResponse({ ...queue(), revision }));
  }
});

test('status decoding requires explicit outcome and allowed actions without inventing terminal success', () => {
  const status = {
    revision: 2,
    queue_id: 'q',
    outcome: null,
    allowed_actions: [],
    install_id: 'i',
    operation_id: 'o',
    done: false,
    view_model: progress,
  };
  assert.equal(installs.installStatusResponse(status).outcome, null);
  assert.equal(installs.installStatusResponse({ ...status, outcome: 'cancelled' }).outcome, 'cancelled');
  assert.throws(() => installs.installStatusResponse({ ...status, outcome: undefined }));
  assert.throws(() => installs.installStatusResponse({ ...status, allowed_actions: undefined }));
});

function session(overrides = {}) {
  return {
    instance_id: 'instance-1',
    session_id: 'session-1',
    launched_at: '2026-09-08T08:00:00Z',
    revision: 4,
    view_model: {
      state_id: 'running',
      label: 'Playing',
      progress_pct: 100,
      terminal: false,
      playing: true,
      process_live: true,
      can_stop: true,
    },
    notice: null,
    outcome: null,
    ...overrides,
  };
}

test('session hydration restores backend process controls and excludes explicitly terminal sessions', () => {
  const active = session();
  const terminal = session({
    instance_id: 'ended',
    session_id: 'ended-session',
    view_model: { ...active.view_model, terminal: true, playing: false, process_live: false, can_stop: false },
    outcome: { kind: 'clean', reason: 'clean_exit', summary: 'Game closed' },
  });
  const parsed = sessions.launchSessionsResponse({ sessions: [active, terminal] });
  assert.equal(parsed['instance-1'].sessionId, 'session-1');
  assert.equal(parsed['instance-1'].statusRevision, 4);
  assert.equal(parsed['instance-1'].viewModel.can_stop, true);
  assert.equal(parsed.ended, undefined);
  assert.throws(() => sessions.launchSessionsResponse({ sessions: [active, active] }));
  assert.throws(() => sessions.launchSessionsResponse({ sessions: [session({ revision: -1 })] }));
  assert.throws(() => sessions.launchSessionsResponse({ sessions: [session({ launched_at: 'invalid' })] }));
});

/** @param {{queueFailure?: boolean, registryFailure?: boolean, sessionFailure?: boolean, activeSessions?: ReturnType<typeof session>[], optionalFailure?: boolean}} [options] */
function bootstrapHarness({ queueFailure = false, registryFailure = false, sessionFailure = false, activeSessions = [], optionalFailure = false } = {}) {
  /** @type {Record<string, {value: unknown}> & {launchSessions: {value: ReturnType<typeof sessions.launchSessionsResponse>}}} */
  const store = {
    ...Object.fromEntries(
      [
        'appVersion',
        'bootstrapError',
        'bootstrapState',
        'config',
        'devMode',
        'instances',
        'lastInstanceId',
        'systemInfo',
        'versions',
      ].map((key) => [key, signal(/** @type {unknown} */ (null))]),
    ),
    launchSessions: signal(/** @type {ReturnType<typeof sessions.launchSessionsResponse>} */ ({})),
    launchState: signal({ status: 'idle' }),
  };
  /** @type {Array<[string, string]>} */
  const calls = [];
  /** @type {Array<[string, string]>} */
  const reconnects = [];
  let failQueue = queueFailure;
  let failRegistry = registryFailure;
  let failSessions = sessionFailure;
  let queueAttempts = 0;
  /** @type {Record<string, unknown>} */
  const values = {
    '/config': { onboarding_done: true },
    '/status': { dev_mode: true, setup_required: false },
    '/system': {},
    '/music/status': { count: 0 },
    '/versions': { versions: [] },
    '/instances': { instances: [{ id: 'instance-1', name: 'World' }], last_instance_id: 'instance-1' },
    '/launch/sessions': { sessions: activeSessions },
    '/install/queue': {
      queue_epoch: 'bootstrap', revision: 1, registry_revision: 0, active: null, items: [], latest_failure: null,
      view_model: { state_id: 'empty', status_label: 'Idle', title: 'Downloads', summary: '', queued_count: 0,
        queued_count_label: '0', queued_item_label: 'Queued', section_title: 'Queue', empty_title: 'Empty', empty_summary: '' },
    },
  };
  const api = {
    initializeApiBase: async () => {},
    api: /** @param {string} method @param {string} path */ async (method, path) => {
      calls.push([method, path]);
      if (path === '/install/queue') {
        queueAttempts++;
        if (failQueue) throw new Error('Queue state unavailable');
      }
      if (failRegistry && path === '/instances') throw new Error('Registry state unavailable');
      if (failSessions && path === '/launch/sessions') throw new Error('Session state unavailable');
      if (optionalFailure && ['/system', '/music/status'].includes(path)) throw new Error('Unavailable');
      assert.ok(Object.prototype.hasOwnProperty.call(values, path), path);
      return values[path];
    },
  };
  const downloads = loadSource('src/machines/downloads.ts', {
    '../api': api, '../utils': { errMessage: String, showError() {} }, '../toast': { toast() {} },
    '../loaders/api': { connectInstallQueueSSE: () => () => {} }, '../store': store,
    '../content-activity': { markContentChanged() {} }, '../dto-install': installs,
    '../dto-core': { versionsResponse: /** @param {unknown} value */ (value) => value,
      instancesResponse: /** @param {unknown} value */ (value) => value },
    '../install-item': loadSource('src/install-item.ts'),
    './download-view-models': loadSource('src/machines/download-view-models.ts'),
  });
  const bootstrap = loadSource(
    'src/bootstrap.ts',
    {
      './api': api,
      './App': { preloadDeferredViews() {} },
      './dto-contract': contract,
      './dto-core': Object.fromEntries(
        [
          'configResponse',
          'instancesResponse',
          'launcherStatusResponse',
          'musicStatusResponse',
          'systemInfoResponse',
          'versionsResponse',
        ].map((name) => [name, /** @param {unknown} value */ (value) => value]),
      ),
      './machines/downloads': downloads,
      './launch': {
        reconnectLaunchSession: /** @param {string} id @param {string} name */ (id, name) => {
          assert.ok(store.launchSessions.value);
          assert.equal(store.launchSessions.value[id].sessionId, 'session-1');
          reconnects.push([id, name]);
        },
      },
      './launch-response-adapters': sessions,
      './music': { Music: { setTrackCount() {}, applyConfig() {}, enabled: false } },
      './native': { getNativeAppVersion: async () => null, hasNativeDesktopRuntime: () => false },
      './preferences/persistence': {},
      './sound': {},
      './player-skin': { refreshAccountSkin() {} },
      './state': { local: { theme: 'obsidian' } },
      './store': store,
      './startup-warnings': { startupWarningMessages: () => [] },
      './theme': { applyConfigTheme() {} },
      './toast': { toast() {} },
      './ui-state': { showOnboardingOverlay: signal(false) },
      './updater': { scheduleAutoUpdateCheck() {} },
      './utils': { errMessage: /** @param {Error} error */ (error) => error.message },
    },
    { window: { requestIdleCallback() {}, addEventListener() {} } },
  );
  return {
    bootstrap,
    store,
    calls,
    reconnects,
    recoverQueue() {
      failQueue = false;
    },
    recoverRegistry() {
      failRegistry = false;
    },
    recoverSessions() {
      failSessions = false;
    },
    changeReadiness() {
      values['/instances'] = { instances: [{ id: 'instance-1', name: 'Updated world' }], last_instance_id: 'instance-1' };
    },
    queueAttempts: () => queueAttempts,
  };
}

test('bootstrap refuses false readiness on failed queue hydration and joins concurrent retry callers', async () => {
  const harness = bootstrapHarness({ queueFailure: true });
  const first = harness.bootstrap.startApplicationBootstrap();
  assert.equal(harness.bootstrap.startApplicationBootstrap(), first);
  await first;
  assert.equal(harness.store.bootstrapState.value, 'error');
  assert.equal(harness.store.bootstrapError.value, 'Queue state unavailable');
  assert.equal(harness.queueAttempts(), 1);
  harness.recoverQueue();
  await harness.bootstrap.startApplicationBootstrap();
  assert.equal(harness.store.bootstrapState.value, 'ready');
  assert.equal(harness.store.bootstrapError.value, null);
  assert.equal(harness.queueAttempts(), 2);
});

test('bootstrap reconnects authoritative sessions and tolerates optional audio/system failures', async () => {
  const harness = bootstrapHarness({ activeSessions: [session()], optionalFailure: true });
  await harness.bootstrap.startApplicationBootstrap();
  assert.equal(harness.store.bootstrapState.value, 'ready');
  assert.deepEqual(harness.reconnects, [['instance-1', 'World']]);
  assert.ok(harness.calls.every(([method]) => method === 'GET'));
  assert.equal(harness.calls.filter(([, path]) => path === '/instances').length, 1);
  assert.equal(harness.calls.filter(([, path]) => path === '/versions').length, 1);
});

test('bootstrap uses the install owner initial projection and retries its failure without becoming ready', async () => {
  const harness = bootstrapHarness({ registryFailure: true, activeSessions: [session()] });
  await harness.bootstrap.startApplicationBootstrap();
  assert.equal(harness.store.bootstrapState.value, 'error');
  assert.equal(harness.store.bootstrapError.value, 'Registry state unavailable');
  assert.equal(harness.store.instances.value, null);
  assert.deepEqual(harness.reconnects, []);
  assert.equal(harness.calls.filter(([, path]) => path === '/instances').length, 1);
  harness.recoverRegistry();
  await harness.bootstrap.startApplicationBootstrap();
  assert.equal(harness.store.bootstrapState.value, 'ready');
  assert.deepEqual(harness.reconnects, [['instance-1', 'World']]);
  assert.equal(harness.calls.filter(([, path]) => path === '/instances').length, 2);
  assert.equal(harness.calls.filter(([, path]) => path === '/versions').length, 2);
});

test('a bootstrap retry refreshes readiness even when only the parallel session read failed and the queue cursor is unchanged', async () => {
  const harness = bootstrapHarness({ sessionFailure: true, activeSessions: [session()] });
  await harness.bootstrap.startApplicationBootstrap();
  await new Promise((done) => setImmediate(done));
  assert.equal(harness.store.bootstrapState.value, 'error');
  assert.equal(harness.store.bootstrapError.value, 'Session state unavailable');
  assert.equal(harness.calls.filter(([, path]) => path === '/instances').length, 1);
  harness.changeReadiness();
  harness.recoverSessions();
  await harness.bootstrap.startApplicationBootstrap();
  assert.equal(harness.store.bootstrapState.value, 'ready');
  assert.deepEqual(harness.reconnects, [['instance-1', 'Updated world']]);
  assert.equal(harness.calls.filter(([, path]) => path === '/instances').length, 2);
});

test('failure controls require an affirmative backend enabled action', () => {
  const Button = () => null;
  const IconButton = () => null;
  const { DownloadFailureNotice } = loadSource('src/ui/DownloadFailureNotice.tsx', {
    './download-failure-notice.css': {},
    './Atoms': { Button, IconButton, Pill: () => null },
    './Icons': { Icon: () => null },
  });
  /** @param {unknown} tree @returns {Array<{ type: unknown, props: Record<string, unknown> }>} */
  function nodes(tree) {
    if (tree == null || typeof tree !== 'object') return [];
    if (Array.isArray(tree)) return tree.flatMap(nodes);
    assert.ok('type' in tree && 'props' in tree);
    const node = /** @type {{ type: unknown, props: Record<string, unknown> }} */ (tree);
    return [node, ...nodes(node.props?.children)];
  }
  // Deliberately bypass the validated DTO to exercise missing-action safeguards.
  const malformedFailure = /** @type {import('../../src/machines/downloads').DownloadFailure} */ (
    /** @type {unknown} */ ({
      displayName: 'Minecraft',
      failedAt: 0,
      viewModel: { ...failure, retry_action: undefined, dismiss_action: undefined },
    })
  );
  const tree = DownloadFailureNotice({
    failure: malformedFailure,
    onRetry() {},
    onDismiss() {},
  });
  for (const node of nodes(tree).filter((node) => node.type === Button || node.type === IconButton)) {
    assert.equal(node.props.disabled, true);
  }
});

test('real Topbar status labels use plain punctuation for downloads, sessions and launch preparation', () => {
  const store = {
    instances: signal([{ id: 'instance-1', name: 'Survival', version_id: '1.20.1' }]),
    launchSessions: signal(sessions.launchSessionsResponse({ sessions: [] })),
    launchState: signal(/** @type {import('../../src/store').LaunchState} */ ({ status: 'idle' })),
    versionById: () => undefined,
  };
  const downloads = {
    activeDownload: signal(
      /** @type {import('../../src/machines/downloads').ActiveDownload | null} */ ({
        queueId: 'queue-1',
        kind: 'vanilla',
        item: { versionId: '1.20.1' },
        displayName: '1.20.1',
        pct: 0,
        label: 'Preparing Minecraft',
        phase: 'starting',
        activeStep: null,
        startedAt: 1,
      }),
    ),
    downloadQueue: signal({ items: [], view_model: queue().view_model }),
    downloadFailure: signal(null),
  };
  /** @type {unknown[]} */
  const navigation = [];
  const { Topbar } = loadSource(
    'src/shell/Topbar.tsx',
    {
      'preact/hooks': {
        /** @template T @param {T | (() => T)} initial */
        useState: (initial) => [typeof initial === 'function' ? /** @type {() => T} */ (initial)() : initial, () => {}],
        /** @template T @param {T} value */
        useRef: (value) => ({ current: value }),
        useEffect() {},
        useLayoutEffect() {},
      },
      '../ui/Icons': { Icon: 'Icon' },
      '../ui/Atoms': { IconButton: 'IconButton' },
      './WindowControls': { WindowControls: 'WindowControls' },
      './MusicWidget': { MusicWidget: 'MusicWidget' },
      './UpdateWidget': { UpdateWidget: 'UpdateWidget' },
      '../store': store,
      '../machines/downloads': downloads,
      '../ui-state': {
        route: signal({ name: 'home' }),
        navigate: /** @param {unknown} next */ (next) => navigation.push(next),
      },
      '../updater': {
        updateFlow: signal({ phase: 'idle', percent: null }),
        hasVisibleUpdate: () => false,
        updateFlowActive: () => false,
      },
      '../version-display': { minecraftVersionLabel: () => '' },
      '../native': { hasCustomDragRegion: () => false },
      '../launch-presenters': loadSource('src/launch-presenters.ts'),
    },
    { performance: { now: () => 0 } },
  );
  /** @param {unknown} tree @returns {Array<{ type: unknown, props: Record<string, unknown> }>} */
  function nodes(tree) {
    if (Array.isArray(tree)) return tree.flatMap(nodes);
    if (!tree || typeof tree !== 'object' || !('props' in tree)) return [];
    const node = /** @type {{ type: unknown, props: Record<string, unknown> }} */ (tree);
    return [node, ...nodes(node.props.children)];
  }
  function pill() {
    const status = nodes(Topbar()).find((node) => typeof node.type === 'function' && node.type.name === 'StatusPill');
    assert.ok(status && typeof status.type === 'function');
    return /** @type {{ props: Record<string, unknown> }} */ (status.type(status.props)).props;
  }
  const downloading = pill();
  assert.equal(downloading.title, '1.20.1: Preparing Minecraft, 0%');
  assert.equal(downloading['aria-label'], 'Open downloads. 1.20.1: Preparing Minecraft, 0%');
  assert.equal(typeof downloading.onClick, 'function');
  if (typeof downloading.onClick === 'function') downloading.onClick();
  assert.equal(JSON.stringify(navigation), '[{"name":"downloads"}]');
  downloads.activeDownload.value = null;
  store.launchSessions.value = sessions.launchSessionsResponse({ sessions: [session()] });
  const running = pill();
  assert.equal(running.title, 'Playing: Survival');
  assert.equal(running['aria-label'], 'Open active instance. Playing: Survival');
  store.launchSessions.value = {};
  store.launchState.value = { status: 'preparing', instanceId: 'instance-1', label: 'Preparing launch', pct: 10 };
  const preparing = pill();
  assert.equal(preparing.title, 'Preparing launch: Survival');
  for (const state of [downloading, running, preparing]) {
    assert.doesNotMatch(String(state.title) + String(state['aria-label']), /\u00b7/);
  }
});
