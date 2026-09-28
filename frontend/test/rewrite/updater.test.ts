import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import type { UpdateFlowState, UpdateInfo } from '../../src/types-update';

const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const requireDependency = createRequire(resolve(frontend, 'package.json'));
const ts: typeof import('typescript') = requireDependency('typescript');
const signal = <T>(value: T): { value: T } => ({ value });
const settle = (): Promise<void> => new Promise((done) => setImmediate(done));

function source<T>(path: string, imports: Record<string, unknown> = {}, globals: Record<string, unknown> = {}): T {
  const filename = resolve(frontend, 'src', path);
  const { outputText } = ts.transpileModule(readFileSync(filename, 'utf8'), {
    fileName: filename,
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
  });
  const exports = {};
  vm.runInNewContext(
    outputText,
    {
      exports,
      Error,
      require(id: string): unknown {
        if (id === '@preact/signals') return { signal };
        if (Object.prototype.hasOwnProperty.call(imports, id)) return imports[id];
        throw new Error(`Unreviewed updater test dependency: ${id}`);
      },
      ...globals,
    },
    { filename },
  );
  return exports as T;
}

const dto = source<typeof import('../../src/dto-contract')>('dto-contract.ts');
const updateTypes = source<typeof import('../../src/types-update')>('types-update.ts');

function flow(phase: UpdateFlowState['phase'], message = ''): UpdateFlowState {
  return { phase, version: '1.1.0', received_bytes: 100, total_bytes: 100, percent: 100, message };
}

function info(): UpdateInfo {
  return {
    current_version: '1.0.0',
    latest_version: '1.1.0',
    available: true,
    platform: 'windows',
    arch: 'x86_64',
    kind: 'release-asset',
    install_mode: 'in-app',
    notes_url: '',
    action_url: '',
    checksum_url: null,
    action_label: 'Download update',
    checked_at: '2026-09-27T10:00:00Z',
  };
}

function harness() {
  let nextTimer = 0;
  let restarts = 0;
  let saved = 0;
  const timers = new Map<number, () => void>();
  const calls: { method: string; path: string; body: unknown }[] = [];
  const notices: string[] = [];
  const responses = new Map<string, unknown>([
    ['/update', info()],
    ['/update?force=1', info()],
    ['/update/download', flow('downloading')],
    ['/update/apply', flow('applying')],
    ['/update/flow', flow('applying')],
  ]);
  const local = { lastUpdateCheckAt: '', dismissedUpdateVersion: '' };
  const store = {
    appVersion: signal('1.0.0'),
    bootstrapState: signal('ready'),
    launchState: signal({ status: 'idle' }),
    updateCheckState: signal('idle'),
    updateInfo: signal<UpdateInfo | null>(null),
  };
  const downloads = {
    activeDownload: signal<object | null>(null),
    downloadQueue: signal({ view_model: { queued_count: 0 } }),
  };
  const updater = source<typeof import('../../src/updater')>(
    'updater.ts',
    {
      './state': {
        local,
        canEditPreferences: () => true,
        saveLocalState: () => {
          saved++;
        },
      },
      './api': {
        api: async (method: string, path: string, body?: unknown) => {
          calls.push({ method, path, body });
          assert.ok(responses.has(path), `Unexpected update request ${path}`);
          const response = responses.get(path);
          if (response instanceof Error) throw response;
          return response;
        },
      },
      './toast': { toast: (message: string) => notices.push(message) },
      './native': {
        hasNativeDesktopRuntime: () => true,
        openExternalURL: async () => {},
        requestNativeAppRestart: async () => {
          restarts++;
          return true;
        },
      },
      './store': store,
      './machines/downloads': downloads,
      './sound': { Sound: { ui() {} } },
      './types-update': updateTypes,
      './utils': { errMessage: String },
      './dto-contract': dto,
    },
    {
      __AXIAL_MOCK_API__: false,
      window: {
        setTimeout(callback: () => void): number {
          const id = ++nextTimer;
          timers.set(id, callback);
          return id;
        },
        clearTimeout(id: number): void {
          timers.delete(id);
        },
      },
    },
  );
  return {
    updater,
    store,
    downloads,
    local,
    calls,
    notices,
    responses,
    timers,
    restarts: () => restarts,
    saved: () => saved,
    async poll(): Promise<void> {
      const timer = timers.entries().next().value;
      assert.ok(timer, 'expected pending update polling');
      timers.delete(timer[0]);
      timer[1]();
      await settle();
    },
  };
}

test('force checks retain their query and downloads submit the exact checked version', async () => {
  const h = harness();
  await h.updater.checkForUpdates({ force: true });
  assert.equal(h.calls[0].method, 'GET');
  assert.equal(h.calls[0].path, '/update?force=1');
  assert.equal(h.store.updateCheckState.value, 'ready');
  assert.equal(h.saved(), 1);
  assert.ok(h.local.lastUpdateCheckAt);
  await h.updater.startUpdateDownload();
  assert.equal(h.calls[1].method, 'POST');
  assert.equal(h.calls[1].path, '/update/download');
  assert.equal(JSON.stringify(h.calls[1].body), '{"version":"1.1.0"}');
  assert.equal(h.updater.updateFlow.value.phase, 'downloading');
  assert.equal(h.timers.size, 1);
});

test('accepted apply polls through installation and interrupted reads before restarting', async () => {
  const h = harness();
  h.updater.updateFlow.value = flow('ready');
  await h.updater.applyUpdateAndRestart();
  assert.equal(h.updater.updateFlow.value.phase, 'applying');
  assert.equal(h.restarts(), 0);
  await h.poll();
  assert.equal(h.restarts(), 0);
  h.responses.set('/update/flow', new Error('API draining'));
  await h.poll();
  assert.equal(h.restarts(), 0);
  assert.equal(h.timers.size, 1);
  h.responses.set('/update/flow', flow('restart-pending'));
  await h.poll();
  assert.equal(h.restarts(), 1);
  assert.equal(h.updater.updateRestartRequested.value, true);
  assert.equal(h.timers.size, 0);
  assert.equal(h.calls.filter((call) => call.path === '/update/apply').length, 1);
});

test('an installed apply response requests a restart immediately', async () => {
  const h = harness();
  h.updater.updateFlow.value = flow('ready');
  h.responses.set('/update/apply', flow('restart-pending'));
  await h.updater.applyUpdateAndRestart();
  assert.equal(h.restarts(), 1);
  assert.equal(h.timers.size, 0);
});

test('failed installation stays visible without restarting', async () => {
  const h = harness();
  h.updater.updateFlow.value = flow('ready');
  await h.updater.applyUpdateAndRestart();
  h.responses.set('/update/flow', flow('failed', 'Installation failed. Restart before continuing.'));
  await h.poll();
  assert.equal(h.restarts(), 0);
  assert.equal(h.updater.updateFlow.value.phase, 'failed');
  assert.equal(h.timers.size, 0);
  assert.ok(h.notices.includes('Installation failed. Restart before continuing.'));
});

test('downloads, queued work and running games refuse apply before the request', async () => {
  for (const activity of ['download', 'queued', 'launch']) {
    const h = harness();
    h.updater.updateFlow.value = flow('ready');
    if (activity === 'download') h.downloads.activeDownload.value = {};
    if (activity === 'queued') h.downloads.downloadQueue.value.view_model.queued_count = 1;
    if (activity === 'launch') h.store.launchState.value.status = 'running';
    await h.updater.applyUpdateAndRestart();
    assert.equal(h.calls.length, 0);
    assert.equal(h.restarts(), 0);
    assert.match(h.notices[0], /Finish downloads and close running games/);
  }
});

test('unsupported checks remain failures without stamping a successful check', async () => {
  const h = harness();
  h.responses.set('/update', new Error('In-app updates are unavailable for this build.'));
  const result = await h.updater.checkForUpdates();
  assert.equal(result, null);
  assert.equal(h.store.updateCheckState.value, 'error');
  assert.equal(h.store.updateInfo.value, null);
  assert.equal(h.local.lastUpdateCheckAt, '');
  assert.equal(h.saved(), 0);
  assert.match(h.notices[0], /Failed to check updates/);
});

test('download and install reaches apply through ready but still waits for installation', async () => {
  const h = harness();
  h.store.updateInfo.value = info();
  await h.updater.downloadAndInstallUpdate();
  h.responses.set('/update/flow', flow('ready'));
  await h.poll();
  assert.equal(h.calls.filter((call) => call.path === '/update/apply').length, 1);
  assert.equal(h.updater.updateFlow.value.phase, 'applying');
  assert.equal(h.restarts(), 0);
  assert.equal(h.timers.size, 1);
  h.responses.set('/update/flow', flow('restart-pending'));
  await h.poll();
  assert.equal(h.restarts(), 1);
});
