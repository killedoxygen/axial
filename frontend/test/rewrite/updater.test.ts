import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import { mockApi } from '../../src/mock/api';
import type { UpdateFlowState, UpdateInfo } from '../../src/types-update';
import type { UpdateFlow } from '../../src/generated/UpdateFlow';
import type { UpdateSnapshot } from '../../src/generated/UpdateSnapshot';

const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const requireDependency = createRequire(resolve(frontend, 'package.json'));
const ts: typeof import('typescript') = requireDependency('typescript');
const jsx: typeof import('preact/jsx-runtime') = requireDependency('preact/jsx-runtime');
const signal = <T>(value: T): { value: T } => ({ value });
const settle = (): Promise<void> => new Promise((done) => setImmediate(done));

function source<T>(path: string, imports: Record<string, unknown> = {}, globals: Record<string, unknown> = {}): T {
  const filename = resolve(frontend, 'src', path);
  const { outputText } = ts.transpileModule(readFileSync(filename, 'utf8'), {
    fileName: filename,
    compilerOptions: {
      module: ts.ModuleKind.CommonJS,
      target: ts.ScriptTarget.ES2020,
      jsx: ts.JsxEmit.ReactJSX,
      jsxImportSource: 'preact',
    },
  });
  const exports = {};
  vm.runInNewContext(
    outputText,
    {
      exports,
      Error,
      require(id: string): unknown {
        if (id === '@preact/signals') return { signal, batch: (action: () => unknown) => action() };
        if (id === 'preact/jsx-runtime') return jsx;
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

function flow(phase: UpdateFlowState['phase'], message = '', terminal = false): UpdateFlow {
  return {
    revision: 10,
    phase,
    version: '1.1.0',
    received_bytes: 100,
    total_bytes: 100,
    percent: 100,
    message,
    supported: true,
    can_check: !terminal && (phase === 'idle' || phase === 'failed'),
    can_download: !terminal && (phase === 'idle' || phase === 'failed'),
    can_apply: phase === 'ready',
    can_restart: terminal || phase === 'restart-pending',
  };
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

function harness(mockRequest?: typeof mockApi) {
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
          if (mockRequest) return mockRequest(method, path, body);
          assert.ok(responses.has(path), `Unexpected update request ${path}`);
          const response = responses.get(path);
          if (response instanceof Error) throw response;
          return typeof response === 'function' ? response() : response;
        },
        isApiError: (error: unknown) => error instanceof Error && error.name === 'ApiError',
      },
      './toast': { toast: (message: string) => notices.push(message) },
      './native': {
        hasNativeDesktopRuntime: () => mockRequest === undefined,
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
      __AXIAL_MOCK_API__: mockRequest !== undefined,
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

test('startup restores a staged update after frontend reload without issuing update commands', async () => {
  const h = harness();
  const retained = {
    info: { ...info(), checksum_url: null },
    flow: flow('ready'),
  } satisfies UpdateSnapshot;
  h.responses.set('/update/snapshot', retained);
  h.responses.set(
    '/update',
    Object.assign(new Error('An update operation is already in progress.'), {
      name: 'ApiError',
      status: 409,
      payload: { code: 'update_busy' },
    }),
  );

  h.updater.scheduleAutoUpdateCheck();
  await h.poll();

  assert.equal(h.updater.updateFlow.value.phase, 'ready');
  assert.equal(h.updater.updateFlow.value.version, '1.1.0');
  assert.equal(JSON.stringify(h.store.updateInfo.value), JSON.stringify(retained.info));
  assert.equal(h.updater.canInstallUpdateInApp(), true);
  assert.deepEqual(h.calls, [{ method: 'GET', path: '/update/snapshot', body: undefined }]);
  assert.equal(h.restarts(), 0);
});

test('mock frontend startup discovers updates through its actual API', async () => {
  const h = harness(mockApi);

  h.updater.scheduleAutoUpdateCheck();
  await h.poll();

  assert.equal(h.store.updateCheckState.value, 'ready');
  assert.equal(h.store.updateInfo.value?.latest_version, '9.9.9');
  assert.equal(h.updater.updateFlow.value.phase, 'idle');
  assert.equal(h.updater.canInstallUpdateInApp(), true);
  assert.deepEqual(
    h.calls.map(({ method, path }) => ({ method, path })),
    [
      { method: 'GET', path: '/update/snapshot' },
      { method: 'GET', path: '/update' },
    ],
  );
  assert.equal(h.restarts(), 0);
});

test('a late startup snapshot cannot replace a newer explicit update check', async () => {
  const h = harness();
  let finishSnapshot: ((snapshot: UpdateSnapshot) => void) | undefined;
  h.responses.set(
    '/update/snapshot',
    () =>
      new Promise<UpdateSnapshot>((resolve) => {
        finishSnapshot = resolve;
      }),
  );
  h.updater.scheduleAutoUpdateCheck();
  await h.poll();
  assert.ok(finishSnapshot);

  const newer = { ...info(), latest_version: '1.2.0' };
  h.responses.set('/update?force=1', newer);
  await h.updater.checkForUpdates({ force: true });
  finishSnapshot({ info: { ...info(), checksum_url: null }, flow: flow('ready') });
  await settle();

  assert.equal(JSON.stringify(h.store.updateInfo.value), JSON.stringify(newer));
  assert.equal(h.store.updateCheckState.value, 'ready');
  assert.equal(h.updater.updateFlow.value.phase, 'idle');
  assert.deepEqual(
    h.calls.map(({ method, path }) => ({ method, path })),
    [
      { method: 'GET', path: '/update/snapshot' },
      { method: 'GET', path: '/update?force=1' },
    ],
  );
  assert.equal(h.restarts(), 0);
});

for (const operation of ['download', 'apply'] as const) {
  test(`lost ${operation} response keeps reading through failures without replaying the command`, async () => {
    const h = harness();
    h.store.updateInfo.value = info();
    const initial = operation === 'apply' ? 'ready' : 'idle';
    h.updater.updateFlow.value = flow(initial);
    h.responses.set(`/update/${operation}`, new Error('Response lost after acceptance'));
    h.responses.set('/update/flow', new Error('Status temporarily unavailable'));
    const command = operation === 'apply' ? h.updater.applyUpdateAndRestart : h.updater.startUpdateDownload;
    await command();
    await settle();
    assert.equal(h.calls.filter((call) => call.path === '/update/flow').length, 1);
    assert.equal(h.timers.size, 1, 'unknown outcome must survive the first failed status read');
    await command();
    assert.equal(h.calls.filter((call) => call.method === 'POST').length, 1);
    h.responses.set('/update/flow', flow(initial));
    await h.poll();
    assert.equal(h.timers.size, 1, 'a pre-acceptance phase does not settle a lost response');
    await command();
    assert.equal(h.calls.filter((call) => call.method === 'POST').length, 1);
    h.responses.set('/update/flow', { ...flow(operation === 'apply' ? 'applying' : 'downloading'), revision: 11 });
    await h.poll();
    assert.equal(h.restarts(), 0);
    h.responses.set('/update/flow', { ...flow(operation === 'apply' ? 'restart-pending' : 'ready'), revision: 12 });
    await h.poll();
    assert.equal(h.updater.updateFlow.value.phase, operation === 'apply' ? 'restart-pending' : 'ready');
    assert.equal(h.restarts(), operation === 'apply' ? 1 : 0);
    assert.equal(h.timers.size, 0);
    assert.equal(h.calls.filter((call) => call.method === 'POST').length, 1);
  });
}

test('a returned apply refusal reconciles read failures before allowing an explicit retry', async () => {
  const h = harness();
  h.updater.updateFlow.value = flow('ready');
  h.responses.set(
    '/update/apply',
    Object.assign(new Error('Application work is active.'), {
      name: 'ApiError',
      status: 502,
      payload: { code: 'update_failed' },
    }),
  );
  h.responses.set('/update/flow', new Error('Status temporarily unavailable'));
  await h.updater.applyUpdateAndRestart();
  await settle();
  assert.equal(h.timers.size, 1);
  h.responses.set('/update/flow', flow('ready'));
  await h.poll();
  assert.equal(h.timers.size, 0);
  assert.equal(h.restarts(), 0);
  assert.equal(h.calls.filter((call) => call.method === 'POST').length, 1);
  h.responses.set('/update/apply', { ...flow('restart-pending'), revision: 12 });
  await h.updater.applyUpdateAndRestart();
  assert.equal(h.calls.filter((call) => call.method === 'POST').length, 2);
  assert.equal(h.restarts(), 1);
});

test('download-and-install retains its intent through a lost download response', async () => {
  const h = harness();
  h.store.updateInfo.value = info();
  h.updater.updateFlow.value = flow('idle');
  h.responses.set('/update/download', new Error('Response lost after acceptance'));
  h.responses.set('/update/flow', { ...flow('ready'), revision: 11 });
  h.responses.set('/update/apply', { ...flow('applying'), revision: 12 });
  await h.updater.downloadAndInstallUpdate();
  await settle();
  assert.equal(h.updater.updateFlow.value.phase, 'applying');
  assert.equal(h.calls.filter((call) => call.path === '/update/download').length, 1);
  assert.equal(h.calls.filter((call) => call.path === '/update/apply').length, 1);
  assert.equal(h.restarts(), 0);
  h.responses.set('/update/flow', { ...flow('restart-pending'), revision: 13 });
  await h.poll();
  assert.equal(h.restarts(), 1);
});

test('recovery refuses stale and unrelated flow evidence and serializes pending commands', async () => {
  const h = harness();
  h.updater.updateFlow.value = flow('ready');
  let loseResponse: ((error: Error) => void) | undefined;
  h.responses.set(
    '/update/apply',
    () =>
      new Promise((_resolve, reject) => {
        loseResponse = reject;
      }),
  );
  const request = h.updater.applyUpdateAndRestart();
  await h.updater.applyUpdateAndRestart();
  assert.equal(h.calls.length, 1);
  assert.ok(loseResponse);
  h.responses.set('/update/flow', { ...flow('restart-pending'), revision: 9 });
  loseResponse(new Error('Response lost'));
  await request;
  await settle();
  assert.equal(h.updater.updateFlow.value.phase, 'ready');
  assert.equal(h.restarts(), 0);
  h.responses.set('/update/flow', { ...flow('restart-pending'), version: '1.2.0', revision: 12 });
  await h.poll();
  assert.equal(h.restarts(), 0);
  assert.equal(h.updater.updateFlow.value.phase, 'ready');
  h.responses.set('/update/flow', { ...flow('failed', 'Installation failed.', true), revision: 12 });
  await h.poll();
  assert.equal(h.updater.updateFlow.value.phase, 'failed');
  assert.equal(h.timers.size, 0);
  assert.equal(h.restarts(), 0);
  assert.equal(h.calls.filter((call) => call.method === 'POST').length, 1);
});

test('flow decoding preserves owner action flags and rejects invalid recovery evidence', () => {
  const h = harness();
  const terminal = h.updater.updateFlowFromResponse(flow('failed', 'Installation failed.', true));
  assert.equal(terminal.can_restart, true);
  assert.equal(terminal.can_download, false);
  for (const invalid of [
    { can_restart: 'true' },
    { can_download: undefined },
    { revision: -1 },
    { revision: 1.5 },
    { revision: Number.MAX_SAFE_INTEGER + 1 },
  ])
    assert.throws(() => h.updater.updateFlowFromResponse({ ...flow('failed'), ...invalid }));
});

interface Node {
  type: unknown;
  props: Record<string, unknown>;
}

function nodes(value: unknown): Node[] {
  if (Array.isArray(value)) return value.flatMap(nodes);
  if (!value || typeof value !== 'object' || !('props' in value)) return [];
  const node = value as Node;
  if (typeof node.type === 'function') return nodes(node.type(node.props));
  return [node, ...Object.values(node.props).flatMap(nodes)];
}

function renderControls(h: ReturnType<typeof harness>, surface: 'widget' | 'about'): Node[] {
  const prefix = surface === 'widget' ? '../' : '../../';
  const imports = {
    'preact/hooks': {
      useState: () => [true, () => {}],
      useRef: () => ({ current: null }),
      useEffect() {},
    },
    [`${prefix}updater`]: h.updater,
    [`${prefix}store`]: h.store,
    [`${prefix}ui/Atoms`]: { Button: 'Button' },
    [`${prefix}ui/Icons`]: { Icon: 'Icon' },
    [`${prefix}ui/SettingsSheet`]: { SettingRow: 'SettingRow', SettingsSection: 'SettingsSection' },
    [`${prefix}format`]: { formatBytes: String },
    [`${prefix}native`]: { hasNativeDesktopRuntime: () => true },
    [`${prefix}toast`]: { toast() {} },
    [`${prefix}utils`]: { errMessage: String },
  };
  return nodes(
    surface === 'widget'
      ? source<typeof import('../../src/shell/UpdateWidget')>('shell/UpdateWidget.tsx', imports).UpdateWidget()
      : source<typeof import('../../src/views/settings/AboutSettingsSection')>(
          'views/settings/AboutSettingsSection.tsx',
          imports,
        ).AboutSettingsSection(),
  );
}

for (const surface of ['widget', 'about'] as const) {
  test(`${surface} installation failure offers restart while a failed download can retry`, async () => {
    for (const terminal of [true, false]) {
      const h = harness();
      h.store.updateInfo.value = info();
      h.updater.updateFlow.value = h.updater.updateFlowFromResponse(flow('failed', 'Update failed.', terminal));
      const primary = renderControls(h, surface).find(
        (node) => node.type === 'Button' && node.props.variant === 'primary',
      );
      assert.ok(primary);
      assert.equal(primary.props.children, terminal ? 'Restart now' : 'Try again');
      (primary.props.onClick as () => void)();
      await settle();
      assert.equal(h.restarts(), terminal ? 1 : 0);
      assert.equal(h.calls.filter((call) => call.path === '/update/download').length, terminal ? 0 : 1);
    }
  });
}

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
