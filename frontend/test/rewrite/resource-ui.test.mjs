import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { basename, dirname, resolve } from 'node:path';
import { before, test } from 'node:test';

/** @typedef {typeof import('../../src/views/instance/resources') & typeof import('../../src/views/instance/bulk-actions') & typeof import('../../src/views/instance/world-actions') & typeof import('../../src/views/instance/screenshot-actions') & typeof import('../../src/views/instance/mod-actions') & typeof import('../../src/views/instance/mod-provenance-cache')} ResourceActions */
/** @typedef {Parameters<typeof import('../../src/api').api>} ApiCall */
/** @typedef {import('../../src/types-content').ContentSelection} ContentSelection */
/** @typedef {import('../../src/views/instance/mod-provenance-cache').ModProvenance} ModProvenance */
/**
 * @typedef {object} ResourceIo
 * @property {Parameters<typeof import('../../src/toast').toast>[]} notices
 * @property {ApiCall[]} calls
 * @property {unknown[][]} folders
 * @property {unknown[][]} navigation
 * @property {(...args: ApiCall) => Promise<unknown>} api
 * @property {() => Promise<string | null>} choice
 * @property {typeof import('../../src/ui/Dialog').prompt} prompt
 * @property {() => Promise<unknown>} list
 * @property {() => Promise<unknown>} updates
 * @property {(...args: Parameters<typeof import('../../src/content').installContent>) => Promise<unknown>} install
 * @property {(...args: Parameters<typeof import('../../src/content').uninstallContents>) => Promise<unknown>} uninstall
 * @property {() => Promise<void>} queue
 */

const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const require = createRequire(resolve(frontend, 'package.json'));
let esbuildPath;
try {
  esbuildPath = require.resolve('esbuild');
} catch {
  // The rewrite initially has no installed dependency tree. Read the retained
  // tool only; never write build output or dependency files into the baseline.
  esbuildPath = require.resolve('../../../legacy/frontend/node_modules/esbuild');
}
const { build } = /** @type {typeof import('esbuild')} */ (require(esbuildPath));
const dependencyRoot = resolve(dirname(esbuildPath), '../../');
/** @type {Record<string, string>} */
const boundaries = {
  api: `export const api = (...args) => io.api(...args); export const apiResourceUrl = path => 'https://local.invalid/api/v1' + path;`,
  toast: `export const toast = (...args) => io.notices.push(args);`,
  Dialog: `export const prompt = (...args) => io.prompt(...args); export const showChoice = (...args) => io.choice(...args);`,
  content: `export const listInstanceContent = (...args) => io.list(...args); export const checkContentUpdates = (...args) => io.updates(...args); export const installContent = (...args) => io.install(...args); export const uninstallContents = (...args) => io.uninstall(...args);`,
  downloads: `export const applyInstallQueueResponse = (...args) => io.queue(...args);`,
  'ui-state': `export const navigate = (...args) => io.navigation.push(args);`,
  'instance-actions': `export const openInstanceFolder = (...args) => io.folders.push(args);`,
  utils: `export const errMessage = error => error instanceof Error ? error.message : String(error); export const modBaseName = name => name.replace(/\\.disabled$/, '');`,
};
let bundleText = '';
before(async () => {
  const bundle = await build({
    stdin: {
      contents: [
        'resources',
        'bulk-actions',
        'world-actions',
        'screenshot-actions',
        'mod-actions',
        'mod-provenance-cache',
      ]
        .map((name) => `export * from './src/views/instance/${name}';`)
        .join('\n'),
      resolveDir: frontend,
    },
    bundle: true,
    write: false,
    platform: 'node',
    format: 'cjs',
    nodePaths: [dependencyRoot],
    logLevel: 'silent',
    plugins: [
      {
        name: 'resource-io-boundaries',
        setup(builder) {
          builder.onResolve({ filter: /./ }, (args) => {
            const name = args.path.split('/').pop();
            return name && Object.prototype.hasOwnProperty.call(boundaries, name)
              ? { path: name, namespace: 'boundary' }
              : undefined;
          });
          builder.onLoad({ filter: /./, namespace: 'boundary' }, (args) => ({ contents: boundaries[args.path] }));
        },
      },
    ],
  });
  assert.ok(bundle.outputFiles);
  bundleText = bundle.outputFiles[0].text;
});

/** @param {Partial<ResourceIo>} [overrides] */
function setup(overrides = {}) {
  /** @type {ResourceIo} */
  const io = {
    notices: [],
    calls: [],
    folders: [],
    navigation: [],
    api: async (...args) => {
      io.calls.push(args);
      return { status: 'ok' };
    },
    choice: async () => 'delete',
    prompt: async () => null,
    list: async () => ({ entries: [] }),
    updates: async () => ({ updates: [] }),
    install: async () => ({ accepted: true }),
    uninstall: async () => ({ accepted: true }),
    queue: async () => undefined,
    ...overrides,
  };
  const module = { exports: {} };
  assert.ok(bundleText, 'Resource actions were bundled by the setup hook');
  new Function('module', 'exports', 'require', 'io', bundleText)(module, module.exports, require, io);
  return { actions: /** @type {ResourceActions} */ (module.exports), io };
}

/** @template T @returns {{ promise: Promise<T>, resolve: (value: T) => void, reject: (reason: unknown) => void }} */
function deferred() {
  /** @type {((value: T) => void) | undefined} */
  // Promise executors run synchronously before this helper returns.
  let resolve;
  /** @type {((reason: unknown) => void) | undefined} */
  let reject;
  /** @type {Promise<T>} */
  const promise = new Promise((yes, no) => {
    resolve = yes;
    reject = no;
  });
  assert.ok(resolve);
  assert.ok(reject);
  return { promise, resolve, reject };
}

/** @type {import('../../src/types-instance').EnrichedInstance} */
const instance = {
  id: 'instance A',
  name: 'Resource fixture',
  version_id: 'fixture',
  created_at: '2026-09-08T12:00:00Z',
  java_selection: { kind: 'inherited' },
  revision: 1,
  version_display: {
    loader_key: 'vanilla',
    loader_label: 'Vanilla',
    minecraft_label: 'Fixture',
    loader_version_label: '',
    loader_detail_label: '',
    summary_label: 'Fixture',
    supports_mods: true,
  },
  launchable: true,
  launch_action: { state_id: 'ready', label: 'Launch', tone: 'ok', launchable: true, primary_action: 'launch' },
  saves_count: 0,
  mods_count: 0,
  resource_count: 0,
  shader_count: 0,
};
const shot = { name: 'photo #1.png', size: 25, modified_at: '2026-09-08T12:00:00Z' };

/** @param {string} name @param {boolean} [enabled] @returns {import('../../src/types-instance').InstanceMod} */
function mod(name, enabled = true) {
  return { name, enabled, size: 25, modified_at: shot.modified_at };
}

/** @param {ResourceActions} actions @returns {string} */
function mutationError(actions) {
  const state = actions.resourceMutationState(instance.id);
  assert.equal(state.status, 'error');
  assert.ok(state.status === 'error');
  return state.error;
}

test('resource commands require an explicit acknowledgement and preserve backend errors', () => {
  const { actions } = setup();
  for (const value of [undefined, null, [], {}, { status: 'pending' }, { status: false }]) {
    assert.throws(() => actions.requireResourceCommandSuccess(value, 'Delete'));
  }
  assert.throws(
    () => actions.requireResourceCommandSuccess({ status: 'ok', error: 'Instance busy' }, 'Delete'),
    /Instance busy/,
  );
  assert.deepEqual(actions.requireResourceCommandSuccess({ status: 'ok' }, 'Delete'), { status: 'ok' });
});

test('cancelled screenshot deletion never submits or clears selection', async () => {
  const { actions, io } = setup({ choice: async () => null });
  let done = 0;
  await actions.deleteScreenshots(instance, [shot], () => done++);
  assert.equal(io.calls.length, 0);
  assert.equal(done, 0);
  assert.equal(actions.resourceMutationState(instance.id).status, 'idle');
});

test('pending confirmation refuses duplicate actions on the same instance only', async () => {
  const confirm = deferred();
  const { actions, io } = setup({ choice: () => confirm.promise });
  const first = actions.deleteScreenshots(instance, [shot], () => undefined);
  assert.equal(actions.resourceMutationState(instance.id).status, 'pending');
  await actions.setModEnabled(instance, mod('a.jar'), () => undefined);
  await actions.setModEnabled({ ...instance, id: 'other' }, mod('b.jar'), () => undefined);
  assert.deepEqual(
    io.calls.map((call) => call[1]),
    ['/instances/other/mods/b.jar'],
  );
  confirm.resolve(null);
  await first;
});

test('backend refusal preserves screenshot selection and lightbox success callbacks', async () => {
  const { actions, io } = setup({ api: async () => ({ error: 'Instance is running' }) });
  let done = 0;
  let refreshed = 0;
  await actions.deleteScreenshots(
    instance,
    [shot],
    () => done++,
    () => refreshed++,
  );
  assert.equal(done, 0);
  assert.equal(refreshed, 1);
  assert.equal(actions.resourceMutationState(instance.id).status, 'error');
  assert.match(mutationError(actions), /Deletion confirmed for 0 of 1.*Instance is running/);
  assert.equal(
    io.notices.some(([message]) => message === 'Screenshot deleted'),
    false,
  );
});

test('partial world deletion stops at first failure and refreshes without clearing remaining selection', async () => {
  /** @type {ApiCall[]} */
  const calls = [];
  const { actions } = setup({
    api: async (...args) => {
      calls.push(args);
      if (calls.length === 2) throw new Error('Disk unavailable');
      return { status: 'ok' };
    },
  });
  let done = 0;
  let refreshed = 0;
  await actions.deleteWorlds(
    instance,
    ['first', 'second', 'third'],
    () => done++,
    () => refreshed++,
  );
  assert.equal(done, 0);
  assert.equal(refreshed, 1);
  assert.equal(calls.length, 2);
  assert.match(mutationError(actions), /Deletion confirmed for 1 of 3.*Disk unavailable/);
});

test('lost bulk deletion responses report only confirmed outcomes without replaying', async () => {
  for (const lostResponseAt of [0, 1]) {
    const worlds = new Set(['first', 'second', 'third']);
    /** @type {ApiCall[]} */
    const calls = [];
    const { actions, io } = setup({
      api: async (...args) => {
        calls.push(args);
        assert.equal(args[0], 'DELETE');
        const name = args[1].split('/').pop();
        assert.ok(name && worlds.delete(name));
        if (calls.length === lostResponseAt + 1) throw new TypeError('Response lost');
        return { status: 'ok' };
      },
    });
    let completed = 0;
    let refreshed = 0;
    await actions.deleteWorlds(
      instance,
      ['first', 'second', 'third'],
      () => completed++,
      () => refreshed++,
    );
    assert.deepEqual([...worlds], lostResponseAt === 0 ? ['second', 'third'] : ['third']);
    assert.equal(calls.length, lostResponseAt + 1);
    assert.equal(completed, 0);
    assert.equal(refreshed, 1);
    assert.match(mutationError(actions), new RegExp(`Deletion confirmed for ${lostResponseAt} of 3\\.`));
    assert.match(mutationError(actions), /Refresh the list before trying again.*Response lost/);
    assert.equal(io.notices.length, 1);
    assert.equal(io.notices[0][1], 'error');
  }
});

test('successful bulk deletion submits exact encoded names and clears selection once', async () => {
  const { actions, io } = setup();
  let done = 0;
  await actions.deleteScreenshots(instance, [shot, { ...shot, name: '../untrusted.png' }], () => done++);
  assert.deepEqual(
    io.calls.map((call) => call.slice(0, 2)),
    [
      ['DELETE', '/instances/instance%20A/screenshots/photo%20%231.png'],
      ['DELETE', '/instances/instance%20A/screenshots/..%2Funtrusted.png'],
    ],
  );
  assert.equal(done, 1);
  assert.equal(actions.resourceMutationState(instance.id).status, 'idle');
});

test('screenshot rename uses the backend name and retains file-kind validation', async () => {
  /** @type {((value: string) => string | null) | undefined} */
  let validation;
  const { actions, io } = setup({
    prompt: async (_message, _name, options) => {
      validation = options?.validate;
      return 'draft.jpeg';
    },
    api: async (...args) => {
      io.calls.push(args);
      return { status: 'ok', name: 'confirmed.jpeg' };
    },
  });
  let selected;
  await actions.renameScreenshot(instance, 'old.jpg', (name) => {
    selected = name;
  });
  assert.equal(selected, 'confirmed.jpeg');
  assert.ok(validation);
  assert.equal(validation('photo.jpeg'), null);
  const changedFileType = validation('photo.png');
  const unsupportedFileType = validation('photo.txt');
  assert.ok(changedFileType);
  assert.ok(unsupportedFileType);
  assert.match(changedFileType, /same screenshot file type/);
  assert.match(unsupportedFileType, /PNG, JPG, JPEG, or WEBP/);
  assert.deepEqual(io.calls[0], ['PUT', '/instances/instance%20A/screenshots/old.jpg', { name: 'draft.jpeg' }]);
});

test('malformed rename response cannot advance lightbox or emit success', async () => {
  const { actions, io } = setup({ prompt: async () => 'new.png' });
  let done = 0;
  await actions.renameScreenshot(instance, shot.name, () => done++);
  assert.equal(done, 0);
  assert.equal(actions.resourceMutationState(instance.id).status, 'error');
  assert.equal(
    io.notices.some(([message]) => message === 'Screenshot renamed'),
    false,
  );
});

test('world backup requires receipt fields before reporting its location', async () => {
  const { actions, io } = setup();
  let done = 0;
  await actions.backupWorld(instance, 'My World', () => done++);
  assert.equal(done, 0);
  assert.equal(actions.resourceMutationState(instance.id).status, 'error');
  io.api = async () => ({ status: 'ok', backup: 'My World.zip', location: 'backups/My World.zip' });
  await actions.backupWorld(instance, 'My World', () => done++);
  assert.equal(done, 1);
  assert.equal(io.notices[io.notices.length - 1][0], 'World backed up to backups/My World.zip');
});

test('bulk enable only changes selected mods whose state differs', async () => {
  const { actions, io } = setup();
  let done = 0;
  await actions.setModsEnabled(instance, [mod('a.jar'), mod('b.jar.disabled', false)], true, () => done++);
  assert.deepEqual(io.calls, [['PUT', '/instances/instance%20A/mods/b.jar.disabled', { enabled: true }]]);
  assert.equal(done, 1);
});

test('mixed mod deletion routes managed entries through content ownership', async () => {
  /** @type {Parameters<ResourceIo['uninstall']>[]} */
  const removals = [];
  const { actions, io } = setup({
    uninstall: async (...args) => {
      removals.push(args);
      return {};
    },
  });
  await actions.deleteMods(
    instance,
    [mod('managed.jar.disabled', false), mod('local.jar')],
    () => undefined,
    new Map([
      [
        'managed.jar',
        {
          canonical_id: 'modrinth:owned',
          kind: 'mod',
          provider: 'modrinth',
          project_id: 'owned',
          version_id: 'current',
          filename: 'managed.jar',
          enabled: false,
        },
      ],
    ]),
  );
  assert.deepEqual(removals, [[instance.id, ['modrinth:owned']]]);
  assert.deepEqual(io.calls, [['DELETE', '/instances/instance%20A/mods/local.jar']]);
});

test('failed mod update queue preserves accepted batch count without resubmitting', async () => {
  /** @type {ContentSelection[][]} */
  const submissions = [];
  const { actions } = setup({
    install: async (_id, items) => {
      submissions.push(items);
      if (submissions.length === 2) throw new Error('Provider unavailable');
      return {};
    },
  });
  await actions.applyModUpdates(
    instance,
    Array.from({ length: 45 }, (_, index) => ({
      canonical_id: `modrinth:${index}`,
      kind: 'mod',
      current_version_id: 'current',
      latest_version_id: `v${index}`,
      latest_version_number: `v${index}`,
    })),
  );
  assert.deepEqual(
    submissions.map((items) => items.length),
    [40, 5],
  );
  assert.match(mutationError(actions), /Queued 40 of 45.*Provider unavailable/);
});

test('cache eviction cannot let a prior provenance request replace the next incarnation', () => {
  const { actions } = setup();
  const stale = actions.beginModProvenanceRefresh(instance.id);
  actions.clearModProvenance(instance.id);
  const current = actions.beginModProvenanceRefresh(instance.id);
  assert.notEqual(stale, current);
  assert.equal(actions.isCurrentModProvenanceRefresh(instance.id, stale), false);
  assert.equal(actions.isCurrentModProvenanceRefresh(instance.id, current), true);
});

test('failed update check retains listed mod details and an explicit failure', async () => {
  const { actions } = setup({
    list: async () => ({ entries: [{ kind: 'mod', filename: 'a.jar', canonical_id: 'modrinth:a' }] }),
    updates: async () => {
      throw new Error('Offline');
    },
  });
  /** @type {ModProvenance[]} */
  const states = [];
  await actions.fetchModProvenance(instance.id, (state) => states.push(state));
  const latest = states[states.length - 1];
  assert.equal(latest.entries.get('a.jar')?.canonical_id, 'modrinth:a');
  assert.equal(latest.updates.size, 0);
  assert.ok(latest.updateError);
  assert.match(latest.updateError, /Could not check mod updates: Offline/);
});

test('late provenance update failure cannot replace newer instance details', async () => {
  const oldUpdates = deferred();
  let checks = 0;
  const { actions } = setup({ updates: async () => (++checks === 1 ? oldUpdates.promise : { updates: [] }) });
  /** @type {ModProvenance[]} */
  const states = [];
  const first = actions.fetchModProvenance(instance.id, (state) => states.push(state));
  await Promise.resolve();
  await actions.fetchModProvenance(instance.id, (state) => states.push(state));
  const before = states.length;
  oldUpdates.reject(new Error('Stale error'));
  await first;
  assert.equal(states.length, before);
  const cached = actions.cachedModProvenance(instance.id);
  assert.ok(cached);
  assert.equal(cached.updateError, undefined);
});

test('screenshot media remains scoped to independently encoded instance and file names', () => {
  const { actions } = setup();
  assert.equal(
    actions.screenshotFileUrl({ ...instance, id: 'id/other' }, 'shot?#.png'),
    'https://local.invalid/api/v1/instances/id%2Fother/screenshots/shot%3F%23.png/file',
  );
});
