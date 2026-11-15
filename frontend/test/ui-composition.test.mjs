import assert from 'node:assert/strict';
import test from 'node:test';
import { apiModule, isApiError, loadSource as load, requireDependency } from './source';
import { enrichedInstance, queueSnapshot } from './wire';

/**
 * @typedef {import('../src/generated/ConfigView').ConfigView} Config
 * @typedef {Partial<Config> & { expected_revision: number, expected_account_selection_revision?: number }} ConfigPatch
 * @typedef {(method: string, path: string, patch?: ConfigPatch) => Promise<unknown>} ConfigApi
 * @typedef {[string, string, number | undefined]} RevisionCall
 */

const signals = /** @type {typeof import('@preact/signals')} */ (requireDependency('@preact/signals'));
const jsxRuntime = /** @type {typeof import('preact/jsx-runtime')} */ (requireDependency('preact/jsx-runtime'));
/** @template T @param {T} value */
const signal = (value) => signals.signal(value);

/**
 * @param {string} path
 * @param {Record<string, unknown>} [stubs]
 * @param {Record<string, unknown>} [globals]
 * @returns {any}
 */
function loadSource(path, stubs = {}, globals = {}) {
  return load(
    path.replace(/^src\//, ''),
    { '@preact/signals': signals, 'preact/jsx-runtime': jsxRuntime, ...stubs },
    { Date, console: { error() {}, warn() {} }, ...globals },
  );
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

const preferences = loadSource('src/app/preferences/local.ts');
const browserPreferenceImports = {
  'app/preferences/local': preferences,
  'platform/native': { hasNativeDesktopRuntime: () => false },
  'app/preferences/persistence': { canEditPreferences: () => true },
};

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

/** @param {ConfigApi} api @param {Config} [initial] */
function configWriterHarness(api, initial = configSnapshot()) {
  const settings = /** @type {typeof import('../src/settings/state')} */ (loadSource('src/settings/state.ts'));
  settings.config.value = initial;
  const writer = loadSource('src/settings/autosave.ts', {
    'preact/hooks': {},
    'settings/state': settings,
    'settings/api': apiModule('settings/api.ts', /** @type {any} */ (api)),
    'app/toast': { toast() {}, errMessage: /** @param {Error} error */ (error) => error.message },
  });
  return { ...writer, config: settings.config, setConfig: settings.setConfig };
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

/** @param {{ lostReply?: boolean, nativeInvoke?: (command: string) => Promise<unknown> }} [options] */
function accountRenameHarness({ lostReply = false, nativeInvoke } = {}) {
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
    skin_action: disabled,
    view_model: { detail: 'Offline identity' },
  };
  let selectionRevision = 4;
  /** @type {Array<[string, string, unknown]>} */
  const calls = [];
  /** @type {Array<Parameters<typeof import('../src/app/toast').toast>>} */
  const toasts = [];
  /** @type {string[]} */
  const errors = [];
  const utils = {
    showError: /** @param {string} message */ (message) => {
      errors.push(message);
    },
  };
  /** @param {string} id @param {boolean} launchable @returns {import('../src/generated/EnrichedInstance').EnrichedInstance} */
  function instance(id, launchable) {
    return enrichedInstance({
      id,
      name: id,
      version_id: '1.21',
      launchable,
      launch_action: {
        state_id: launchable ? 'ready' : 'blocked',
        label: launchable ? 'Launch' : 'Unavailable',
        tone: launchable ? 'ok' : 'warn',
        launchable,
        primary_action: launchable ? 'launch' : 'blocked',
      },
    });
  }
  const settings = /** @type {typeof import('../src/settings/state')} */ (loadSource('src/settings/state.ts'));
  settings.config.value = configSnapshot();
  const instanceState = /** @type {typeof import('../src/instances/state')} */ (loadSource('src/instances/state.ts'));
  instanceState.instances.value = [instance('first', false), instance('second', false)];
  const store = { ...settings, ...instanceState, launchSessions: signal({}) };
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
      return {
        status: 'account_updated',
        selection_revision: selectionRevision,
        account,
        view_model: { summary: 'Offline identity updated.' },
      };
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
        sign_in_available: !!nativeInvoke,
        accounts: [account],
      });
    throw new Error(`Unexpected account request ${method} ${path}`);
  };
  const accountsApi = apiModule('accounts/api.ts', /** @type {any} */ (api));
  const accountState = /** @type {typeof import('../src/accounts/snapshot')} */ (
    loadSource('src/accounts/snapshot.ts')
  );
  const readiness = loadSource(
    'src/instances/readiness.ts',
    {
      'instances/api': apiModule('instances/api.ts', /** @type {any} */ (api)),
      'settings/state': settings,
      'instances/state': instanceState,
      'launch/state': store,
      'app/toast': utils,
    },
    { window: { setTimeout } },
  );
  const machine = /** @type {typeof import('../src/accounts/state')} */ (
    loadSource(
      'src/accounts/state.ts',
      {
        'accounts/snapshot': accountState,
        'settings/state': settings,
        'settings/api': apiModule('settings/api.ts', /** @type {any} */ (api)),
        'platform/transport': { isApiError },
        'platform/native': nativeInvoke
          ? loadSource('src/platform/native.ts', {}, { window: { __TAURI__: { core: { invoke: nativeInvoke } } } })
          : {},
        'accounts/player-name': { promptPlayerName: async () => 'Renamed' },
        'accounts/player-skin': { refreshAccountSkin() {}, moveAccountSkin() {}, forgetAccountSkin() {} },
        'app/toast': {
          ...utils,
          toast: /** @param {Parameters<typeof import('../src/app/toast').toast>} args */ (...args) =>
            toasts.push(args),
        },
        'ui/Dialog': {},
        'accounts/api': accountsApi,
        'instances/readiness': readiness,
      },
      { console: { warn() {} } },
    )
  );
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

/** @param {typeof import('../src/accounts/state')} machine */
function activeAccount(machine) {
  const account = machine.activeAccount();
  assert.ok(account);
  return account;
}

test('offline stage rename uses account fences and accepts the backend-generated replacement identity', async () => {
  const { machine, calls, toasts } = accountRenameHarness();
  await machine.refreshAccountsData();
  const saved = await machine.renameOfflineAccount(activeAccount(machine), 'Renamed');
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
  await machine.renameOfflineAccount(activeAccount(machine));
  assert.equal(machine.accountsNotice.value, null);
  assert.equal(activeAccount(machine).account_id, 'offline-after');
  assert.equal(toasts.length, 1);
});

test('an account change refreshes backend launch availability for every retained instance', async () => {
  const { machine, calls, store } = accountRenameHarness();
  await machine.refreshAccountsData();
  assert.ok(store.instances.value.every((instance) => !instance.launch_action.launchable));
  assert.equal(await machine.renameOfflineAccount(activeAccount(machine), 'Renamed'), true);
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

  assert.equal(await h.machine.renameOfflineAccount(activeAccount(h.machine), 'Renamed'), true);
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
  assert.equal(await h.machine.renameOfflineAccount(activeAccount(h.machine), 'Renamed'), true);
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

test('a lost committed account reply still refreshes readiness without replay or success', async () => {
  const { machine, calls, toasts } = accountRenameHarness({ lostReply: true });
  await machine.refreshAccountsData();
  assert.equal(await machine.renameOfflineAccount(activeAccount(machine), 'Renamed'), false);
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
  assert.equal(await h.machine.renameOfflineAccount(activeAccount(h.machine), 'Renamed'), false);
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
        return { .../** @type {Record<string, unknown>} */ (response), active_account_id: null, accounts: [] };
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
      ['/config', '/accounts', '/config', '/accounts'],
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
  'online-action',
  'config-unavailable',
  'accounts-unavailable',
]) {
  test(`sign-in cancellation still refreshes readiness after ${change}`, async () => {
    const h = accountRenameHarness({ nativeInvoke: async () => ({ status: 'cancelled' }) });
    const actionChange = change === 'online-action';
    let changed = false;
    h.read(async (path, response) => {
      const value = /** @type {Record<string, unknown>} */ (response);
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
            actionChange
              ? {
                  ...account,
                  kind: 'microsoft',
                  login_id: 'current-login',
                  minecraft_profile: profile,
                  minecraft_profile_ready: true,
                  minecraft_ownership_verified: true,
                  online_action: changed ? expired : online,
                }
              : account,
          ),
        };
      }
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
    if (change === 'accounts-unavailable') assert.equal(h.machine.accountsSnapshot.value.state, 'unavailable');
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
  const state = loadSource('src/app/local-state.ts', browserPreferenceImports, { localStorage });
  assert.equal(state.local.theme, 'obsidian');
  state.local.theme = 'birch';
  state.saveLocalState();
  assert.equal(JSON.parse(storedValue(localStorage, state.STORAGE_KEY)).theme, 'birch');
  const first = state.loadLocalState();
  first.selectedSkinsByAccount.a = 'skin';
  assert.equal(state.loadLocalState().selectedSkinsByAccount.a, undefined);
});

test('stored browser preferences validate values and recover with fresh defaults', () => {
  const localStorage = storage([['axial_ui', JSON.stringify({ theme: 'end', sounds: false })]]);
  const state = loadSource('src/app/local-state.ts', browserPreferenceImports, { localStorage });
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
  const ui = loadSource('src/app/route.ts', browserPreferenceImports, { localStorage });
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

/** @param {Partial<import('../src/generated/SessionSnapshot').SessionSnapshot>} [overrides] @returns {import('../src/generated/SessionSnapshot').SessionSnapshot} */
function session(overrides = {}) {
  return {
    instance_id: 'instance-1',
    session_id: 'session-1',
    launched_at: '2026-09-08T08:00:00Z',
    revision: 4,
    phase: 'running',
    started_at_ms: null,
    pid: null,
    process_alive: true,
    stop_allowed: true,
    exit_code: null,
    tree_settled: false,
    output_drained: false,
    boot_observed: true,
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
  const launch = /** @type {typeof import('../src/launch/state')} */ (loadSource('src/launch/state.ts'));
  const active = session();
  const terminal = session({
    instance_id: 'ended',
    session_id: 'ended-session',
    view_model: { ...active.view_model, terminal: true, playing: false, process_live: false, can_stop: false },
    outcome: { kind: 'clean', reason: 'clean_exit', failure_class: null, summary: 'Game closed' },
  });
  const parsed = launch.liveSessions([active, terminal]);
  assert.equal(parsed['instance-1'].sessionId, 'session-1');
  assert.equal(parsed['instance-1'].statusRevision, 4);
  assert.equal(parsed['instance-1'].viewModel.can_stop, true);
  assert.equal(parsed.ended, undefined);
  assert.throws(() => launch.liveSessions([active, active]));
});

/** @param {{queueFailure?: boolean, registryFailure?: boolean, sessionFailure?: boolean, activeSessions?: ReturnType<typeof session>[], optionalFailure?: boolean}} [options] */
function bootstrapHarness({
  queueFailure = false,
  registryFailure = false,
  sessionFailure = false,
  activeSessions = [],
  optionalFailure = false,
} = {}) {
  const appStore = /** @type {typeof import('../src/app/store')} */ (loadSource('src/app/store.ts'));
  const settings = /** @type {typeof import('../src/settings/state')} */ (loadSource('src/settings/state.ts'));
  const instanceState = /** @type {typeof import('../src/instances/state')} */ (loadSource('src/instances/state.ts'));
  const launch = /** @type {typeof import('../src/launch/state')} */ (loadSource('src/launch/state.ts'));
  const store = { ...appStore, ...settings, ...instanceState, ...launch };
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
    '/status': { dev_mode: true, setup_required: false, warnings: [] },
    '/system': {},
    '/music/status': { count: 0 },
    '/versions': { versions: [] },
    '/instances': { instances: [{ id: 'instance-1', name: 'World' }], last_instance_id: 'instance-1' },
    '/launch/sessions': { sessions: activeSessions },
    '/install/queue': queueSnapshot({ queue_epoch: 'bootstrap', revision: 1, registry_revision: 0, failures: [] }),
  };
  const transport = {
    request: /** @param {string} method @param {string} path */ async (method, path) => {
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
  const call = /** @type {any} */ (transport.request);
  const toast = { toast() {}, showError() {}, errMessage: /** @param {Error} error */ (error) => error.message };
  const downloads = loadSource('src/install/queue.ts', {
    'app/toast': toast,
    'install/api': apiModule('install/api.ts', call),
    'instances/api': apiModule('instances/api.ts', call),
    'settings/state': settings,
    'instances/state': instanceState,
    'launch/state': launch,
    'content/activity': { markContentChanged() {} },
    'install/item': loadSource('src/install/item.ts'),
    'install/presenters': loadSource('src/install/presenters.ts'),
  });
  const bootstrap = loadSource(
    'src/app/bootstrap.ts',
    {
      'platform/transport': { initializeApiBase: async () => {} },
      'app/App': { preloadDeferredViews() {} },
      'settings/api': apiModule('settings/api.ts', call),
      'music/api': apiModule('music/api.ts', call),
      'launch/api': apiModule('launch/api.ts', call),
      'install/queue': downloads,
      'launch/controller': {
        reconnectLaunchSession: /** @param {string} id @param {string} name */ (id, name) => {
          assert.ok(store.launchSessions.value);
          assert.equal(store.launchSessions.value[id].sessionId, 'session-1');
          reconnects.push([id, name]);
        },
      },
      'music/music': { Music: { setTrackCount() {}, applyConfig() {}, enabled: false } },
      'platform/native': { getNativeAppVersion: async () => null, hasNativeDesktopRuntime: () => false },
      'app/preferences/persistence': {},
      'app/sound': {},
      'accounts/state': { refreshAccountsData: async () => {} },
      'app/local-state': { local: { theme: 'obsidian' } },
      'app/store': appStore,
      'settings/state': settings,
      'instances/state': instanceState,
      'launch/state': launch,
      'app/theme': { applyConfigTheme() {} },
      'app/toast': toast,
      'app/route': { route: signal({ name: 'home' }), showOnboardingOverlay: signal(false) },
      'update/updater': { scheduleAutoUpdateCheck() {} },
      'home/HomeView': { warmHome: async () => {} },
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
      values['/instances'] = {
        instances: [{ id: 'instance-1', name: 'Updated world' }],
        last_instance_id: 'instance-1',
      };
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
  assert.equal(harness.store.instances.value.length, 0);
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
