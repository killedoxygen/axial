import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import type { ApiEventOptions } from '../../src/backend/events';
import type { InstallQueueStateResponse } from '../../src/types-install';
import type { LaunchSession } from '../../src/types-launch';
import type { EnrichedInstance } from '../../src/types-instance';
import type { Config } from '../../src/types-settings';

const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');
const requireDependency = createRequire(resolve(frontend, 'package.json'));
const ts: typeof import('typescript') = requireDependency('typescript');
const signals: typeof import('@preact/signals') = requireDependency('@preact/signals');

const { signal } = signals;

function source<T extends object>(path: string, imports: Record<string, unknown> = {}, globals: Record<string, unknown> = {}): T {
  const filename = resolve(frontend, 'src', path);
  const output = ts.transpileModule(readFileSync(filename, 'utf8'), {
    fileName: filename,
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
  });
  const exports = {};
  vm.runInNewContext(output.outputText, {
    exports, Error, URLSearchParams, structuredClone,
    require(id: string): unknown {
      if (id === '@preact/signals') return signals;
      if (Object.prototype.hasOwnProperty.call(imports, id)) return imports[id];
      throw new Error(`Unreviewed recovery test dependency: ${id}`);
    },
    ...globals,
  }, { filename });
  return exports as T;
}

function timers() {
  let next = 0;
  const timeouts = new Map<number, { callback: () => void; delay: number }>();
  const intervals = new Map<number, () => void>();
  return {
    timeouts, intervals,
    setTimeout(callback: () => void, delay: number): number { const id = ++next; timeouts.set(id, { callback, delay }); return id; },
    clearTimeout(id: number): void { timeouts.delete(id); },
    setInterval(callback: () => void): number { const id = ++next; intervals.set(id, callback); return id; },
    clearInterval(id: number): void { intervals.delete(id); },
    runTimeout(): void {
      const timer = timeouts.entries().next().value;
      assert.ok(timer, 'expected a pending timeout');
      timeouts.delete(timer[0]); timer[1].callback();
    },
  };
}

function deferred<T>() {
  let resolveValue!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => { resolveValue = resolvePromise; reject = rejectPromise; });
  return { promise, resolve: resolveValue, reject };
}

const flush = (): Promise<void> => new Promise((done) => setImmediate(done));
const contract = source<typeof import('../../src/dto-contract')>('dto-contract.ts');
const installContract = source<typeof import('../../src/dto-install')>('dto-install.ts', { './dto-contract': contract });
const coreContract = source<typeof import('../../src/dto-core')>('dto-core.ts', {
  './dto-contract': contract, './dto-install': installContract,
});
const logContract = source<typeof import('../../src/dto-launch')>('dto-launch.ts', { './dto-contract': contract });
const noticeContract = source<typeof import('../../src/launch-notice-tracker')>('launch-notice-tracker.ts');
const statusContract = source<typeof import('../../src/launch-response-adapters')>('launch-response-adapters.ts', {
  './dto-contract': contract, './launch-notice-tracker': noticeContract,
});

function historicalStatusApi(
  statusCode = 404,
  payload: unknown = { error: 'The instance was not found.', code: 'instance_not_found' },
  path = '/launch/4772f752-5f75-48fb-a1f0-bb484f858ea0/status',
) {
  return source<typeof import('../../src/api')>(
    'api.ts',
    {
      './native': { getNativeApiTransportBootstrap: async () => null },
      './dto-contract': contract,
    },
    {
      URL,
      Headers,
      __AXIAL_WEB_API_BASE__: 'http://127.0.0.1:1',
      __AXIAL_TEST_API_CAPABILITY__: 'fixture-capability',
      __AXIAL_MOCK_API__: false,
      fetch: async (url: string) => {
        assert.equal(url, `http://127.0.0.1:1/api/v1${path}`);
        return new Response(JSON.stringify(payload), {
          status: statusCode,
          headers: { 'Content-Type': 'application/json' },
        });
      },
    },
  );
}
const apiContract = historicalStatusApi();

function status(revision = 1, terminal = false) {
  return {
    session_id: 'session-1', instance_id: 'instance-1', revision,
    launched_at: '2026-09-08T08:00:00Z', notice: null,
    outcome: terminal ? { kind: 'clean', reason: 'clean_exit', summary: 'Game closed' } : null,
    view_model: { state_id: terminal ? 'exited' : 'running', label: terminal ? 'Ended' : 'Playing',
      progress_pct: 100, terminal, playing: !terminal, process_live: !terminal, can_stop: !terminal },
  };
}

function log(sequence: number) { return { sequence, source: 'stdout', text: `line-${sequence}`, truncated: false }; }

function instance(launchable = false): EnrichedInstance {
  return {
    id: 'instance-1', name: 'Example', version_id: '1.21', created_at: '2026-09-08T08:00:00Z',
    java_selection: { kind: 'inherited' },
    revision: 1,
    version_display: { loader_key: 'vanilla', loader_label: 'Vanilla', minecraft_label: '1.21',
      loader_version_label: '', loader_detail_label: '', summary_label: '1.21', supports_mods: false },
    launchable, launch_action: { state_id: launchable ? 'ready' : 'blocked', label: launchable ? 'Launch' : 'Unavailable',
      tone: launchable ? 'ok' : 'warn', launchable, primary_action: launchable ? 'launch' : 'blocked' },
    saves_count: 0, mods_count: 0, resource_count: 0, shader_count: 0, counts_available: false,
  };
}

function configSnapshot(): Config {
  return coreContract.configResponse({ revision: 1, account_selection_revision: 1, username: 'Player', launch_auth_mode: 'offline',
    max_memory_mb: 4096, min_memory_mb: 512, java_path_override: '', window_width: 0, window_height: 0, jvm_preset: '',
    performance_mode: 'vanilla', theme: 'obsidian', custom_hue: null, custom_vibrancy: null, lightness: null,
    onboarding_done: true, telemetry_enabled: false, discord_rpc_enabled: false, discord_rpc_onboarding_seen: true,
    music_enabled: false, music_volume: 50, music_track: 0 });
}

function launchHarness(intentResult?: unknown, running = true) {
  const clock = timers();
  const finalLogs = deferred<unknown>();
  let currentStatus = status();
  let readStatus: () => Promise<unknown> = async () => currentStatus;
  let postLaunch: ((body: unknown) => Promise<unknown>) | undefined;
  let postKill: () => Promise<unknown> = async () => status(6, true);
  let readIntent: (path: string) => Promise<unknown> = async () => intentResult;
  let subscription: ApiEventOptions<unknown> | undefined;
  let closed = 0;
  let connections = 0;
  const calls: string[] = [];
  const errors: string[] = [];
  const sounds: string[] = [];
  const lines: Array<{ source: string; text: string }> = [];
  let readInstance: () => Promise<unknown> = async () => instance(true);
  const store = {
    launchSessions: signal<Record<string, LaunchSession>>(
      statusContract.launchSessionsResponse({ sessions: intentResult === undefined && running ? [currentStatus] : [] }),
    ),
    launchState: signal<import('../../src/store').LaunchState>({ status: 'idle' }),
    config: signal<Config | null>(null),
    instances: signal([instance()]),
    selectedInstance: signal(instance(true)),
    instanceLaunchDrafts: signal({}),
    selectedInstanceId: signal('instance-1'),
    launchNotices: signal({}),
    versions: signal<unknown[]>([]),
    lastInstanceId: signal<string | null>(null),
  };
  const actions = source<typeof import('../../src/actions')>('actions.ts', {
    './store': store,
    './launch-response-adapters': statusContract,
  });
  const api = {
    isApiError: apiContract.isApiError,
    api: async (method: string, path: string, body?: unknown) => {
      if (path === '/launch' && (intentResult !== undefined || postLaunch)) {
        assert.equal(method, 'POST');
        calls.push(path);
        if (postLaunch) return postLaunch(body);
        throw new Error('Launch response lost');
      }
      if (method === 'POST' && path.endsWith('/kill')) {
        calls.push(path);
        return postKill();
      }
      assert.equal(method, 'GET');
      calls.push(path);
      if (path.startsWith('/launch/intents/')) return readIntent(path);
      if (path.endsWith('/logs')) return finalLogs.promise;
      if (path.endsWith('/status')) return readStatus();
      if (path === '/instances/instance-1') return readInstance();
      if (path === '/instances') return { instances: [await readInstance()], last_instance_id: null };
      throw new Error(`Unexpected launch read ${path}`);
    },
  };
  const readiness = source<typeof import('../../src/instance-readiness')>(
    'instance-readiness.ts',
    {
      './api': api,
      './dto-core': coreContract,
      './store': store,
      './utils': { showError: (message: string) => errors.push(message) },
    },
    { window: clock },
  );
  const launch = source<typeof import('../../src/launch')>(
    'launch.ts',
    {
      './api': api,
      './backend/events': {
        subscribeApiEvents: (_path: string, options: ApiEventOptions<unknown>) => {
          connections++;
          subscription = options;
          return () => {
            closed++;
          };
        },
      },
      './sound': { Sound: { init() {}, ui: (sound: string) => sounds.push(sound) } },
      './music': { Music: { suppress() {}, unsuppress() {} } },
      './utils': {
        appendLog: (source: string, text: string) => lines.push({ source, text }),
        showError: (message: string) => errors.push(message),
        errMessage: String,
      },
      './store': store,
      './actions': actions,
      './launch-notice-tracker': noticeContract,
      './launch-response-adapters': statusContract,
      './dto-contract': contract,
      './dto-core': {},
      './dto-launch': logContract,
      './instance-readiness': readiness,
    },
    { ...clock, window: clock, crypto: { randomUUID: () => '9eb6ce58-c29a-44bc-a290-aea8973e9bdb' } },
  );
  if (intentResult === undefined && running) launch.reconnectLaunchSession('instance-1', 'Example');
  return {
    store,
    actions,
    lines,
    calls,
    errors,
    sounds,
    clock,
    finalLogs,
    readiness,
    launch,
    closed: () => closed,
    connections: () => connections,
    postLaunch(next: (body: unknown) => Promise<unknown>): void {
      postLaunch = next;
    },
    postKill(next: typeof postKill): void {
      postKill = next;
    },
    readIntent(next: typeof readIntent): void {
      readIntent = next;
    },
    readInstance(next: typeof readInstance): void {
      readInstance = next;
    },
    readStatus(next: typeof readStatus): void {
      readStatus = next;
    },
    poll(): void {
      for (const poll of clock.intervals.values()) poll();
    },
    emit(value: unknown, event: string): void {
      assert.ok(subscription);
      subscription.onValue(value, event, null);
    },
    interruptStream(): void {
      assert.ok(subscription);
      subscription.onError?.(new Error('Interrupted'));
    },
    pollTerminal(): void {
      currentStatus = status(2, true);
      for (const poll of clock.intervals.values()) poll();
    },
  };
}

test('already Playing adoption and bootstrap reconnect project recency once without replaying launch sound', async () => {
  for (const entrypoint of ['adoption', 'reconnect']) {
    const h = launchHarness(undefined, false);
    h.store.instances.value = [{ ...instance(), name: 'Current name', java_path: '/fixture/java' }];
    if (entrypoint === 'adoption') await h.launch.adoptLaunchSession('session-1');
    else {
      h.actions.confirmLaunch('instance-1', statusContract.launchSessionsResponse({ sessions: [status()] })['instance-1']);
      h.launch.reconnectLaunchSession('instance-1', 'Example');
    }
    await flush();
    const projected = h.store.instances.value[0];
    assert.equal(projected.last_played_at, status().launched_at);
    assert.equal(projected.name, 'Current name');
    assert.equal(projected.java_path, '/fixture/java');
    h.emit(status(2), 'status');
    h.emit(status(2), 'status');
    h.launch.reconnectLaunchSession('instance-1', 'Example');
    assert.equal(h.store.instances.value[0], projected);
    assert.deepEqual(h.sounds, []);
    assert.equal(h.calls.some((path) => path.startsWith('/instances')), false);
  }
});

for (const change of ['settings edit', 'newer recency', 'replacement session', 'session lifetime', 'instance removal']) {
  test(`first Playing recency respects current ownership after ${change}`, async () => {
    const h = launchHarness(undefined, false);
    const starting = { ...status(), view_model: { ...status().view_model, state_id: 'starting', label: 'Starting', playing: false } };
    h.readStatus(async () => starting);
    await h.launch.adoptLaunchSession('session-1');
    await flush();
    assert.equal(h.store.instances.value[0].last_played_at, undefined);
    if (change === 'settings edit') {
      h.actions.updateInstanceInList({ ...instance(), name: 'Renamed while starting', java_path: '/fixture/new-java' });
    } else if (change === 'newer recency') {
      h.actions.updateInstanceInList({ ...instance(), last_played_at: '2026-10-04T08:00:00Z' });
    } else if (change === 'instance removal') h.store.instances.value = [];
    else {
      h.actions.confirmLaunch('instance-1', statusContract.launchSessionsResponse({ sessions: [{ ...status(), session_id: 'session-2' }] })['instance-1']);
      if (change === 'session lifetime') h.actions.endSessionIfCurrent('instance-1', 'session-2');
    }
    const before = h.store.instances.value;
    h.emit(status(2), 'status');
    if (change === 'settings edit') {
      assert.equal(h.store.instances.value[0].last_played_at, status().launched_at);
      assert.equal(h.store.instances.value[0].name, 'Renamed while starting');
      assert.equal(h.store.instances.value[0].java_path, '/fixture/new-java');
    } else assert.equal(h.store.instances.value, before);
    const after = h.store.instances.value;
    h.emit(status(3), 'status');
    assert.equal(h.store.instances.value, after);
    assert.deepEqual(h.sounds, []);
  });
}

test('ordinary accepted Play retains one launch sound and accepted recency', async () => {
  const h = launchHarness({ state: 'accepted', session: status() });
  await h.launch.launchGame();
  await flush();
  assert.equal(h.store.instances.value[0].last_played_at, status().launched_at);
  h.emit(status(2), 'status');
  assert.deepEqual(h.sounds, ['launchSuccess']);
});

function settledIntent() {
  return {
    state: 'accepted',
    session: {
      session_id: '4772f752-5f75-48fb-a1f0-bb484f858ea0',
      instance_id: 'instance-1',
      revision: 1,
      phase: 'exited',
      launched_at: '2026-09-08T08:00:00.000Z',
      started_at_ms: null,
      pid: null,
      process_alive: false,
      stop_allowed: false,
      exit_code: 0,
      tree_settled: true,
      output_drained: true,
      boot_observed: true,
      outcome: {
        kind: 'stopped',
        reason: 'launcher_stopped',
        failure_class: null,
        summary: 'The game was stopped from the launcher.',
      },
      notice: { message: 'The game was stopped from the launcher.', tone: 'info' },
      view_model: {
        state_id: 'exited',
        label: 'Session ended',
        progress_pct: 100,
        terminal: true,
        playing: false,
        process_live: false,
        can_stop: false,
      },
    },
  };
}

async function acceptedPlay() {
  const h = launchHarness(undefined, false);
  const sessionId = '4772f752-5f75-48fb-a1f0-bb484f858ea0';
  const intentPath = '/launch/intents/9eb6ce58-c29a-44bc-a290-aea8973e9bdb';
  const accepted = { ...status(7), session_id: sessionId, launched_at: '2026-09-08T08:00:00.000Z' };
  h.postLaunch(async (body) => {
    assert.equal((body as { instance_id: string }).instance_id, 'instance-1');
    assert.equal((body as { intent_key: string }).intent_key, '9eb6ce58-c29a-44bc-a290-aea8973e9bdb');
    return accepted;
  });
  h.readStatus(async () => accepted);
  await h.launch.launchGame();
  await flush();
  assert.equal(h.store.launchSessions.value['instance-1'].statusRevision, 7);
  assert.equal(h.store.launchSessions.value['instance-1'].viewModel.playing, true);
  const missing = historicalStatusApi();
  const settled = historicalStatusApi(200, settledIntent(), intentPath);
  h.readStatus(() => missing.api('GET', `/launch/${sessionId}/status`));
  h.readIntent((path) => settled.api('GET', path));
  return { ...h, sessionId, intentPath, accepted };
}

test('accepted Play settles from its original terminal intent after API reopen without replaying launch', async () => {
  const h = await acceptedPlay();
  h.finalLogs.reject(new Error('Historical live logs are unavailable after reopen'));
  // Retain the rejection until the existing terminal owner requests the log tail.
  void h.finalLogs.promise.catch(() => {});
  h.interruptStream();
  h.poll();
  await flush();
  await flush();

  assert.equal(
    h.store.launchSessions.value['instance-1'],
    undefined,
    'authenticated original settlement must clear stale Playing even when the cold snapshot revision is lower',
  );
  assert.equal(h.store.instances.value[0].launch_action.launchable, true);
  assert.equal(h.clock.intervals.size, 0);
  assert.equal(h.closed(), 1);
  assert.equal(h.calls.filter((path) => path === '/launch').length, 1);
  assert.equal(h.calls.filter((path) => path === h.intentPath).length, 1);
  assert.equal(
    h.calls.some((path) => path.endsWith('/kill')),
    false,
  );
  assert.deepEqual(h.sounds, ['launchSuccess']);
  assert.equal(h.lines.filter((line) => line.text === 'The game was stopped from the launcher.').length, 1);
});

test('a restored Playing session without its original intent cannot infer settlement from absence', async () => {
  const h = launchHarness();
  await flush();
  const sessions = h.store.launchSessions.value;
  const path = '/launch/session-1/status';
  const missing = historicalStatusApi(404, { error: 'The instance was not found.', code: 'instance_not_found' }, path);
  h.readStatus(() => missing.api('GET', path));
  h.poll();
  await flush();
  assert.equal(h.store.launchSessions.value, sessions);
  assert.equal(
    h.calls.some((call) => call.startsWith('/launch/intents/')),
    false,
  );
  assert.equal(
    h.calls.some((call) => call === '/launch' || call.endsWith('/kill') || call.endsWith('/logs')),
    false,
  );
  assert.equal(h.closed(), 0);
});

for (const [code, payload] of [
  [404, { error: 'Unclassified absence' }],
  [404, { error: 'Different refusal', code: 'another_error' }],
  [404, { code: 'instance_not_found' }],
  [503, { error: 'Status unavailable', code: 'instance_not_found' }],
] as const) {
  test(`unclassified or unavailable status cannot authorize an intent read: ${code} ${JSON.stringify(payload)}`, async () => {
    const h = await acceptedPlay();
    const sessions = h.store.launchSessions.value;
    const path = `/launch/${h.sessionId}/status`;
    const response = historicalStatusApi(code, payload, path);
    h.readStatus(() => response.api('GET', path));
    h.poll();
    await flush();
    assert.equal(h.store.launchSessions.value, sessions);
    assert.equal(
      h.calls.some((call) => call.startsWith('/launch/intents/')),
      false,
    );
    assert.equal(h.calls.filter((call) => call === '/launch').length, 1);
    assert.equal(h.closed(), 0);
  });
}

const terminal = settledIntent();
for (const [label, payload] of [
  ['preparing', { state: 'preparing' }],
  ['rejected', { state: 'rejected', error: 'The launch could not be prepared.', code: 'preparation_failed' }],
  [
    'interrupted',
    {
      state: 'interrupted',
      session_id: terminal.session.session_id,
      error: 'The process outcome is unknown.',
      code: 'interrupted',
    },
  ],
  ['missing snapshot', { state: 'accepted' }],
  ['different session', { ...terminal, session: { ...terminal.session, session_id: 'different-session' } }],
  ['different instance', { ...terminal, session: { ...terminal.session, instance_id: 'different-instance' } }],
  ['different launch time', { ...terminal, session: { ...terminal.session, launched_at: '2026-09-08T08:00:01.000Z' } }],
  ['unresolved phase', { ...terminal, session: { ...terminal.session, phase: 'unresolved' } }],
  ['unsettled tree', { ...terminal, session: { ...terminal.session, tree_settled: false } }],
  ['undrained output', { ...terminal, session: { ...terminal.session, output_drained: false } }],
  ['live process', { ...terminal, session: { ...terminal.session, process_alive: true } }],
  ['stop allowed', { ...terminal, session: { ...terminal.session, stop_allowed: true } }],
  ['missing outcome', { ...terminal, session: { ...terminal.session, outcome: null } }],
  ['unsafe revision', { ...terminal, session: { ...terminal.session, revision: Number.MAX_SAFE_INTEGER + 1 } }],
  ...(['playing', 'process_live', 'can_stop'] as const).map(
    (flag) =>
      [
        flag,
        {
          ...terminal,
          session: { ...terminal.session, view_model: { ...terminal.session.view_model, [flag]: true } },
        },
      ] as const,
  ),
] as const) {
  test(`unverified original intent cannot clear Playing: ${label}`, async () => {
    const h = await acceptedPlay();
    const sessions = h.store.launchSessions.value;
    const response = historicalStatusApi(200, payload, h.intentPath);
    h.readIntent((path) => response.api('GET', path));
    h.poll();
    await flush();
    await flush();
    assert.equal(h.calls.filter((path) => path === h.intentPath).length, 1);
    assert.equal(h.store.launchSessions.value, sessions);
    assert.equal(h.store.launchSessions.value['instance-1'].viewModel.playing, true);
    assert.equal(
      h.calls.some((path) => path.endsWith('/logs') || path.endsWith('/kill')),
      false,
    );
    assert.equal(h.calls.filter((path) => path === '/launch').length, 1);
    assert.equal(h.closed(), 0);
    assert.deepEqual(h.errors, []);
  });
}

for (const change of ['replacement session', 'same-session lifetime', 'newer revision'] as const) {
  test(`late original settlement cannot clear ${change}`, async () => {
    const h = await acceptedPlay();
    const reply = deferred<unknown>();
    h.readIntent(() => reply.promise);
    h.poll();
    await flush();
    assert.equal(h.calls.filter((path) => path === h.intentPath).length, 1);
    const current = h.store.launchSessions.value['instance-1'];
    if (change === 'replacement session') {
      h.actions.confirmLaunch('instance-1', { ...current, sessionId: 'session-2', intentKey: 'another-intent' });
    } else if (change === 'same-session lifetime') {
      h.actions.endSessionIfCurrent('instance-1', h.sessionId);
      h.actions.confirmLaunch('instance-1', { ...current });
    } else h.emit({ ...h.accepted, revision: 8 }, 'status');
    const sessions = h.store.launchSessions.value;
    reply.resolve(settledIntent());
    await flush();
    assert.equal(h.store.launchSessions.value, sessions);
    assert.equal(
      h.calls.some((path) => path.endsWith('/logs')),
      false,
    );
    assert.equal(h.calls.filter((path) => path === '/launch').length, 1);
    assert.equal(h.closed(), 0);
  });
}

test('repeated original settlement completes once while another instance is selected and logs are pending', async () => {
  const h = await acceptedPlay();
  const other = { ...instance(true), id: 'instance-2', name: 'Other' };
  h.store.instances.value = [...h.store.instances.value, other];
  h.store.selectedInstance.value = other;
  h.actions.selectInstance('instance-2');
  h.actions.confirmLaunch('instance-2', {
    sessionId: 'session-2',
    launchedAt: h.accepted.launched_at,
    statusRevision: 3,
    viewModel: status(3).view_model,
  });
  const otherSession = h.store.launchSessions.value['instance-2'];
  for (let attempt = 0; attempt < 3; attempt += 1) {
    h.poll();
    await flush();
    await flush();
  }
  assert.equal(h.calls.filter((path) => path.endsWith('/logs')).length, 1);
  assert.equal(h.closed(), 0);
  const settled = h.store.launchSessions.value['instance-1'];
  assert.equal(
    settled.viewModel.playing,
    false,
    'verified terminal proof must remove Playing before final logs arrive',
  );
  assert.equal(settled.viewModel.can_stop, false);
  assert.equal(settled.viewModel.process_live, false);
  assert.equal(settled.viewModel.terminal, true);
  assert.equal(settled.statusRevision, 7, 'cold settlement must not rebase the live revision');
  h.finalLogs.resolve({ entries: [] });
  await flush();
  assert.equal(h.store.launchSessions.value['instance-1'], undefined);
  assert.equal(h.store.launchSessions.value['instance-2'], otherSession);
  assert.equal(h.store.selectedInstance.value, other);
  assert.equal(h.clock.intervals.size, 0);
  assert.equal(h.closed(), 1);
  assert.equal(h.calls.filter((path) => path === '/instances/instance-1').length, 1);
  assert.equal(
    h.calls.some((path) => path.endsWith('/kill')),
    false,
  );
  assert.equal(h.calls.filter((path) => path === '/launch').length, 1);
  assert.equal(h.lines.filter((line) => line.text === 'The game was stopped from the launcher.').length, 1);
  assert.deepEqual(h.sounds, ['launchSuccess']);
});

test('delayed live status cannot restore Playing after verified settlement while final logs are pending', async () => {
  const h = await acceptedPlay();
  h.poll();
  await flush();
  await flush();
  assert.equal(h.store.launchSessions.value['instance-1'].viewModel.terminal, true);
  assert.equal(h.calls.filter((path) => path.endsWith('/logs')).length, 1);
  assert.equal(h.closed(), 0);

  h.emit({ ...h.accepted, revision: 8 }, 'status');
  const current = h.store.launchSessions.value['instance-1'];
  assert.equal(
    current.viewModel.playing,
    false,
    'a delayed higher live revision cannot undo authenticated terminal settlement',
  );
  assert.equal(current.viewModel.can_stop, false);
  assert.equal(current.viewModel.process_live, false);
  assert.equal(current.viewModel.terminal, true);
  assert.equal(current.statusRevision, 7);
  assert.equal(h.calls.filter((path) => path.endsWith('/logs')).length, 1);
  h.finalLogs.resolve({ entries: [] });
  await flush();
  assert.equal(h.store.launchSessions.value['instance-1'], undefined);
  assert.equal(h.closed(), 1);
  assert.equal(h.lines.filter((line) => line.text === 'The game was stopped from the launcher.').length, 1);
  assert.deepEqual(h.sounds, ['launchSuccess']);
});

test('a pending Stop response cannot undo verified settlement while final logs are pending', async () => {
  const h = await acceptedPlay();
  const reply = deferred<unknown>();
  h.postKill(() => reply.promise);
  const stopping = h.launch.killGame();
  assert.equal(h.calls.filter((path) => path === `/launch/${h.sessionId}/kill`).length, 1);
  h.poll();
  await flush();
  await flush();
  assert.equal(h.store.launchSessions.value['instance-1'].viewModel.terminal, true);
  assert.equal(h.calls.filter((path) => path.endsWith('/logs')).length, 1);

  reply.resolve({
    ...h.accepted,
    revision: 8,
    phase: 'stopping',
    process_alive: true,
    stop_allowed: true,
    tree_settled: false,
    output_drained: false,
    view_model: { ...h.accepted.view_model, state_id: 'stopping', label: 'Stopping Minecraft', playing: false },
  });
  await stopping;
  const current = h.store.launchSessions.value['instance-1'];
  assert.equal(
    current.viewModel.terminal,
    true,
    'a delayed Stop response cannot undo authenticated terminal settlement',
  );
  assert.equal(current.viewModel.process_live, false);
  assert.equal(current.viewModel.can_stop, false);
  assert.equal(current.statusRevision, 7);
  assert.deepEqual(h.errors, []);
  h.finalLogs.resolve({ entries: [] });
  await flush();
  assert.equal(h.store.launchSessions.value['instance-1'], undefined);
  assert.equal(h.closed(), 1);
  assert.equal(h.calls.filter((path) => path.endsWith('/logs')).length, 1);
});

for (const failure of ['refused', 'response lost'] as const) {
  test(`an obsolete Stop ${failure} cannot publish a warning after verified settlement`, async () => {
    const h = await acceptedPlay();
    const reply = deferred<unknown>();
    h.postKill(() => reply.promise);
    const stopping = h.launch.killGame();
    h.poll();
    await flush();
    await flush();
    const settled = h.store.launchSessions.value['instance-1'];
    assert.equal(settled.viewModel.terminal, true);
    if (failure === 'refused') {
      const path = `/launch/${h.sessionId}/kill`;
      const response = historicalStatusApi(404, { error: 'The instance was not found.', code: 'instance_not_found' }, path);
      await response.api('POST', path).catch(reply.reject);
    } else reply.reject(new Error('Stop response lost'));
    await stopping;
    assert.equal(h.store.launchSessions.value['instance-1'], settled);
    assert.deepEqual(h.errors, []);
    assert.equal(h.calls.filter((path) => path.endsWith('/kill')).length, 1);
    h.finalLogs.resolve({ entries: [] });
    await flush();
    assert.equal(h.store.launchSessions.value['instance-1'], undefined);
    assert.equal(h.closed(), 1);
    assert.equal(h.calls.filter((path) => path.endsWith('/logs')).length, 1);
  });
}

test('original final-log completion cannot clear a replacement launch or refresh its readiness', async () => {
  const h = await acceptedPlay();
  h.poll();
  await flush();
  await flush();
  assert.equal(h.calls.filter((path) => path.endsWith('/logs')).length, 1);
  h.actions.confirmLaunch('instance-1', {
    sessionId: 'session-2',
    intentKey: 'another-intent',
    launchedAt: '2026-09-08T08:00:01.000Z',
    statusRevision: 1,
    viewModel: status().view_model,
  });
  const sessions = h.store.launchSessions.value;
  const rows = h.store.instances.value;
  h.finalLogs.resolve({ entries: [log(1)] });
  await flush();
  assert.equal(h.store.launchSessions.value, sessions);
  assert.equal(h.store.instances.value, rows);
  assert.equal(
    h.calls.some((path) => path.startsWith('/instances')),
    false,
  );
  assert.equal(
    h.lines.some((line) => line.text === 'The game was stopped from the launcher.'),
    false,
  );
  assert.equal(
    h.lines.some((line) => line.text === 'line-1'),
    false,
  );
  assert.equal(h.closed(), 1);
});

test('a known external session is adopted once and ordinary Stop targets its exact identity', async () => {
  const h = launchHarness(undefined, false);
  h.actions.startLaunch('other-instance');
  const preparing = h.store.launchState.value;
  await h.launch.adoptLaunchSession('session-1');
  await flush();
  assert.equal(h.store.launchSessions.value['instance-1'].sessionId, 'session-1');
  assert.equal(h.store.launchState.value, preparing, 'adoption must preserve unrelated preparation');
  assert.equal(h.connections(), 1);
  assert.equal(h.clock.intervals.size, 1);
  const reads = h.calls.length;
  h.actions.updateLaunchSessionState('instance-1', { stopping: true });
  h.emit(status(5), 'status');
  await h.launch.adoptLaunchSession('session-1');
  assert.equal(h.calls.length, reads);
  assert.equal(h.connections(), 1);
  assert.equal(h.store.launchSessions.value['instance-1'].statusRevision, 5);
  assert.equal(h.store.launchSessions.value['instance-1'].stopping, true);

  h.actions.updateLaunchSessionState('instance-1', { stopping: false });
  await h.launch.killGame();
  assert.equal(h.calls.filter((path) => path === '/launch/session-1/kill').length, 1);
  h.finalLogs.resolve({ entries: [] });
  await flush();
  assert.equal(h.store.launchSessions.value['instance-1'], undefined);
  assert.equal(h.closed(), 1);
  assert.equal(h.store.launchState.value, preparing);
});

test('concurrent duplicate adoption never creates a second connection or replaces newer state', async () => {
  const h = launchHarness(undefined, false);
  const reply = deferred<unknown>();
  h.readStatus(() => reply.promise);
  const first = h.launch.adoptLaunchSession('session-1');
  const duplicate = h.launch.adoptLaunchSession('session-1');
  reply.resolve(status());
  await Promise.all([first, duplicate]);
  await flush();
  assert.equal(h.connections(), 1);
  assert.equal(h.clock.intervals.size, 1);
  assert.equal(h.store.launchSessions.value['instance-1'].sessionId, 'session-1');
});

for (const [label, value] of [
  ['malformed', { error: 'private provider detail' }],
  ['mismatched identity', { ...status(), session_id: 'different-session' }],
  ['invalid launch time', { ...status(), launched_at: 'invalid' }],
  ['unsafe revision', { ...status(), revision: Number.MAX_SAFE_INTEGER + 1 }],
] as const) {
  test(`invalid external session response cannot publish controls: ${label}`, async () => {
    const h = launchHarness(undefined, false);
    const sessions = h.store.launchSessions.value;
    h.readStatus(async () => value);
    await h.launch.adoptLaunchSession('session-1');
    assert.equal(h.store.launchSessions.value, sessions);
    assert.equal(h.connections(), 0);
    assert.equal(h.errors.length, 1);
    assert.doesNotMatch(h.errors[0], /private provider detail/);
    assert.equal(h.clock.timeouts.size + h.clock.intervals.size, 0);
  });
}

test('an already terminal external session is not resurrected or connected', async () => {
  const h = launchHarness(undefined, false);
  const sessions = h.store.launchSessions.value;
  h.readStatus(async () => status(2, true));
  await h.launch.adoptLaunchSession('session-1');
  assert.equal(h.store.launchSessions.value, sessions);
  assert.equal(h.connections(), 0);
  assert.equal(h.errors.length, 0);
});

test('typed absence of a historical session is quiet and preserves any current game', async () => {
  for (const running of [false, true]) {
    const h = launchHarness(undefined, running);
    await flush();
    const response = historicalStatusApi();
    h.readStatus(() => response.api('GET', '/launch/4772f752-5f75-48fb-a1f0-bb484f858ea0/status'));
    const sessions = h.store.launchSessions.value;
    const preparation = h.store.launchState.value;
    const connections = h.connections();
    await h.launch.adoptLaunchSession('4772f752-5f75-48fb-a1f0-bb484f858ea0');
    assert.equal(h.calls[h.calls.length - 1], '/launch/4772f752-5f75-48fb-a1f0-bb484f858ea0/status');
    assert.equal(h.store.launchSessions.value, sessions);
    assert.equal(h.store.launchState.value, preparation);
    assert.equal(h.connections(), connections);
    assert.equal(h.closed(), 0);
    assert.deepEqual(h.errors, []);
  }
});

test('unclassified 404 and unavailable historical status remain visible safe failures', async () => {
  for (const [code, payload] of [
    [404, { error: 'private provider detail' }],
    [404, { error: 'private provider detail', code: 'another_error' }],
    [404, { code: 'instance_not_found' }],
    [503, { error: 'private provider detail', code: 'instance_not_found' }],
  ] as const) {
    const h = launchHarness(undefined, false);
    const sessions = h.store.launchSessions.value;
    const response = historicalStatusApi(code, payload);
    h.readStatus(() => response.api('GET', '/launch/4772f752-5f75-48fb-a1f0-bb484f858ea0/status'));
    await h.launch.adoptLaunchSession('4772f752-5f75-48fb-a1f0-bb484f858ea0');
    assert.equal(h.store.launchSessions.value, sessions);
    assert.equal(h.connections(), 0);
    assert.equal(h.errors.length, 1);
    assert.doesNotMatch(h.errors[0], /private provider detail/);
  }
});

test('late historical absence cannot clear or reconnect a newer live projection', async () => {
  const h = launchHarness();
  await flush();
  const pending = deferred<unknown>();
  h.readStatus(() => pending.promise);
  const adoption = h.launch.adoptLaunchSession('4772f752-5f75-48fb-a1f0-bb484f858ea0');
  h.emit(status(5), 'status');
  const sessions = h.store.launchSessions.value;
  const response = historicalStatusApi();
  await response.api('GET', '/launch/4772f752-5f75-48fb-a1f0-bb484f858ea0/status').catch(pending.reject);
  await adoption;
  assert.equal(h.store.launchSessions.value, sessions);
  assert.equal(h.store.launchSessions.value['instance-1'].statusRevision, 5);
  assert.equal(h.connections(), 1);
  assert.equal(h.closed(), 0);
  assert.deepEqual(h.errors, []);
});

for (const change of [
  'replacement session',
  'session lifetime',
  'preparation',
  'preparation lifetime',
  'instance removal',
] as const) {
  test(`a delayed external session read cannot overwrite a newer ${change}`, async () => {
    const h = launchHarness(undefined, false);
    const reply = deferred<unknown>();
    h.readStatus(() => reply.promise);
    const adoption = h.launch.adoptLaunchSession('session-1');
    if (change === 'replacement session' || change === 'session lifetime') {
      const replacement = statusContract.launchSessionsResponse({
        sessions: [{ ...status(), session_id: 'session-2' }],
      });
      h.actions.confirmLaunch('instance-1', replacement['instance-1']);
      if (change === 'session lifetime') h.actions.endSessionIfCurrent('instance-1', 'session-2');
    } else if (change === 'preparation' || change === 'preparation lifetime') {
      h.actions.startLaunch('instance-1');
      if (change === 'preparation lifetime') h.actions.endLaunchPrep();
    } else h.store.instances.value = [];
    const sessions = h.store.launchSessions.value;
    const preparing = h.store.launchState.value;
    reply.resolve(status());
    await adoption;
    assert.equal(h.store.launchSessions.value, sessions);
    assert.equal(h.store.launchState.value, preparing);
    assert.equal(h.connections(), 0);
  });
}

test('external adoption cannot replace an existing session or strand same-instance preparation', async () => {
  for (const occupied of ['session', 'preparation'] as const) {
    const h = launchHarness(undefined, false);
    if (occupied === 'session') {
      const replacement = statusContract.launchSessionsResponse({
        sessions: [{ ...status(), session_id: 'session-2' }],
      });
      h.actions.confirmLaunch('instance-1', replacement['instance-1']);
    } else h.actions.startLaunch('instance-1');
    const sessions = h.store.launchSessions.value;
    const preparing = h.store.launchState.value;
    await h.launch.adoptLaunchSession('session-1');
    assert.equal(h.store.launchSessions.value, sessions);
    assert.equal(h.store.launchState.value, preparing);
    assert.equal(h.connections(), 0);
  }
});

test('an obsolete external session read failure cannot surface after a preparation lifetime', async () => {
  const h = launchHarness(undefined, false);
  const reply = deferred<unknown>();
  h.readStatus(() => reply.promise);
  const adoption = h.launch.adoptLaunchSession('session-1');
  h.actions.startLaunch('instance-1');
  h.actions.endLaunchPrep();
  reply.reject(new Error('private provider detail'));
  await adoption;
  assert.deepEqual(h.errors, []);
  assert.equal(h.connections(), 0);
});

test('an interrupted durable launch stops recovery polling and prevents a fresh launch', async () => {
  const harness = launchHarness({ state: 'interrupted', session_id: 'session-1', error: 'Its process outcome is unknown.' });
  await harness.launch.launchGame();
  await flush();
  assert.equal(harness.store.launchState.value.status, 'preparing');
  assert.match((harness.store.launchState.value as { label?: string }).label ?? '', /outcome unknown/);
  assert.equal(Object.keys(harness.store.launchSessions.value).length, 0);
  assert.equal(harness.clock.timeouts.size, 0);
  assert.equal(harness.errors[harness.errors.length - 1], 'Its process outcome is unknown.');
  await harness.launch.launchGame();
  assert.equal(harness.calls.filter((path) => path === '/launch').length, 1);
});

test('an interrupted reply without its session identity cannot end status reconciliation', async () => {
  const harness = launchHarness({ state: 'interrupted' });
  await harness.launch.launchGame();
  await flush();
  assert.equal(harness.store.launchState.value.status, 'preparing');
  assert.equal(harness.clock.timeouts.size, 1);
  harness.clock.runTimeout();
  await flush();
  assert.equal(harness.calls.filter((path) => path === '/launch').length, 1);
  assert.equal(Object.keys(harness.store.launchSessions.value).length, 0);
});

test('replayed launch history is appended once per session sequence', async () => {
  const harness = launchHarness();
  await flush();
  for (const sequence of [1, 2, 1, 2, 3]) harness.emit(log(sequence), 'log');
  assert.deepEqual(harness.lines.map((line) => line.text), ['line-1', 'line-2', 'line-3']);
  assert.throws(() => harness.emit({ ...log(4), sequence: -1 }, 'log'), /sequence/);
  harness.emit(log(5), 'log');
  assert.match(harness.lines[3].text, /no longer available/);
  assert.equal(harness.lines[4].text, 'line-5');
  harness.emit({ ...log(6), truncated: true }, 'log');
  assert.equal(harness.lines[5].text, 'line-6 [truncated]');
});

test('terminal polling drains the retained log tail before closing the stream', async () => {
  const harness = launchHarness();
  await flush();
  harness.emit(log(1), 'log');
  harness.pollTerminal();
  await flush();
  assert.equal(harness.closed(), 0);
  assert.ok(harness.store.launchSessions.value['instance-1']);
  harness.emit(log(2), 'log');
  harness.emit(status(2, true), 'status');
  harness.finalLogs.resolve({ entries: [log(1), log(2), log(3)] });
  await flush();
  assert.deepEqual(harness.lines.map((line) => line.text), ['line-1', 'line-2', 'line-3', 'Game closed']);
  assert.equal(harness.store.launchSessions.value['instance-1'], undefined);
  assert.equal(harness.closed(), 1);
  assert.equal(harness.calls.filter((path) => path.endsWith('/logs')).length, 1);
  assert.equal(harness.store.instances.value[0].launch_action.label, 'Launch');
  assert.equal(harness.calls.filter((path) => path === '/instances/instance-1').length, 1);
  assert.equal(harness.clock.timeouts.size, 0);
});

test('a failed final log read remains visible and does not strand an ended session', async () => {
  const harness = launchHarness();
  await flush();
  harness.emit(status(2, true), 'status');
  await flush();
  harness.finalLogs.reject(new Error('Unavailable'));
  await flush();
  assert.match(harness.lines[0].text, /final log history could not be refreshed/);
  assert.equal(harness.lines[1].text, 'Game closed');
  assert.equal(harness.closed(), 1);
  assert.equal(harness.store.launchSessions.value['instance-1'], undefined);
});

test('a launch stream failure preserves polling convergence and terminal cleanup', async () => {
  const harness = launchHarness();
  await flush();
  harness.interruptStream();
  assert.match(harness.lines[0].text, /Checking session status continues/);
  assert.equal(harness.clock.intervals.size, 1);
  harness.pollTerminal();
  await flush();
  harness.finalLogs.resolve({ entries: [log(1)] });
  await flush();
  assert.equal(harness.store.launchSessions.value['instance-1'], undefined);
  assert.equal(harness.clock.intervals.size, 0);
  assert.equal(harness.closed(), 1);
});

test('final log timeout is bounded and a late reply cannot affect a replacement session', async () => {
  const harness = launchHarness();
  await flush();
  harness.emit(status(2, true), 'status');
  await flush();
  assert.equal([...harness.clock.timeouts.values()][0].delay, 5000);
  harness.clock.runTimeout();
  await flush();
  harness.store.launchSessions.value = { 'instance-1': { sessionId: 'session-2', launchedAt: status().launched_at,
    statusRevision: 1, viewModel: status().view_model } };
  const count = harness.lines.length;
  harness.finalLogs.resolve({ entries: [log(1)] });
  await flush();
  assert.equal(harness.lines.length, count);
  assert.equal(harness.store.launchSessions.value['instance-1'].sessionId, 'session-2');
});

test('terminal readiness rechecks once after the launch reservation is released', async () => {
  const harness = launchHarness();
  let reads = 0;
  harness.readInstance(async () => instance(++reads > 1));
  harness.emit(status(2, true), 'status');
  harness.finalLogs.resolve({ entries: [] });
  await flush();
  assert.equal(harness.store.launchSessions.value['instance-1'], undefined);
  assert.equal(harness.store.instances.value[0].launch_action.label, 'Unavailable');
  assert.equal([...harness.clock.timeouts.values()][0].delay, 250);
  harness.clock.runTimeout();
  await flush();
  assert.equal(harness.store.instances.value[0].launch_action.label, 'Launch');
  assert.equal(reads, 2);
  assert.equal(harness.clock.timeouts.size, 0);
});

test('a backend refusal remains unavailable after the bounded settlement reads', async () => {
  const harness = launchHarness();
  harness.readInstance(async () => instance());
  harness.emit(status(2, true), 'status');
  harness.finalLogs.resolve({ entries: [] });
  await flush();
  harness.clock.runTimeout();
  await flush();
  assert.equal(harness.store.instances.value[0].launch_action.launchable, false);
  assert.equal(harness.calls.filter((path) => path === '/instances/instance-1').length, 2);
  assert.equal(harness.clock.timeouts.size, 0);
});

test('failed readiness reads surface an error without retaining an ended session or retrying forever', async () => {
  const harness = launchHarness();
  harness.readInstance(async () => { throw new Error('Offline'); });
  harness.emit(status(2, true), 'status');
  harness.finalLogs.resolve({ entries: [] });
  await flush();
  harness.clock.runTimeout();
  await flush();
  assert.equal(harness.store.launchSessions.value['instance-1'], undefined);
  assert.equal(harness.store.instances.value[0].launch_action.launchable, false);
  assert.equal(harness.closed(), 1);
  assert.equal(harness.errors.length, 1);
  assert.match(harness.errors[0], /Could not refresh launch availability/);
  assert.equal(harness.clock.timeouts.size, 0);
});

for (const change of ['edit', 'remove', 'session', 'config'] as const) {
  test(`late terminal readiness cannot overwrite a newer ${change}`, async () => {
    const harness = launchHarness();
    const reply = deferred<unknown>();
    harness.readInstance(() => reply.promise);
    harness.emit(status(2, true), 'status');
    harness.finalLogs.resolve({ entries: [] });
    await flush();
    if (change === 'edit') harness.store.instances.value = [{ ...instance(), name: 'Renamed' }];
    if (change === 'remove') harness.store.instances.value = [];
    if (change === 'config') harness.store.config.value = { ...harness.store.config.value } as Config;
    if (change === 'session') harness.store.launchSessions.value = { 'instance-1': {
      sessionId: 'session-2', launchedAt: status().launched_at, statusRevision: 1, viewModel: status().view_model,
    } };
    const before = harness.store.instances.value;
    reply.resolve(instance(true));
    await flush();
    assert.equal(harness.store.instances.value, before);
    assert.equal(harness.clock.timeouts.size, 0);
  });
}

test('a newer account readiness read supersedes an older terminal readiness response', async () => {
  const harness = launchHarness();
  const oldReply = deferred<unknown>();
  harness.readInstance(() => oldReply.promise);
  harness.emit(status(2, true), 'status');
  harness.finalLogs.resolve({ entries: [] });
  await flush();
  harness.readInstance(async () => instance(true));
  await harness.readiness.refreshInstanceReadiness();
  oldReply.resolve(instance());
  await flush();
  assert.equal(harness.store.instances.value[0].launch_action.label, 'Launch');
  assert.equal(harness.clock.timeouts.size, 0);
});

function queue(epoch: string, revision: number, registryRevision: number): InstallQueueStateResponse {
  return { queue_epoch: epoch, revision, registry_revision: registryRevision, active: null, items: [], latest_failure: null,
    view_model: { state_id: 'empty', status_label: epoch, title: 'Downloads', summary: '', queued_count: 0,
      queued_count_label: '0', queued_item_label: 'Queued', section_title: 'Queue', empty_title: 'Empty', empty_summary: '' } };
}

function downloadsHarness(sharedStore?: {
  versions: { value: unknown[] };
  instances: { value: unknown[] };
  lastInstanceId: { value: string | null };
  config: { value: Config | null };
  launchSessions: { value: Record<string, LaunchSession> };
  launchState: { value: import('../../src/store').LaunchState };
}) {
  const clock = timers();
  const calls: string[] = [];
  const errors: string[] = [];
  let subscription: ((response: InstallQueueStateResponse) => void) | undefined;
  const store = sharedStore ?? {
    versions: signal<unknown[]>([]), instances: signal<unknown[]>([]), lastInstanceId: signal<string | null>(null),
    config: signal<Config | null>(null), launchSessions: signal<Record<string, LaunchSession>>({}),
    launchState: signal<import('../../src/store').LaunchState>({ status: 'idle' }),
  };
  let read: (path: string) => Promise<unknown> = async (path) => path === '/versions'
    ? { versions: ['installed'] } : { instances: ['ready'], last_instance_id: null };
  const machine = source<typeof import('../../src/machines/downloads')>('machines/downloads.ts', {
    '../api': { api: async (method: string, path: string) => { assert.equal(method, 'GET'); calls.push(path); return read(path); } },
    '../utils': { errMessage: String, showError: (message: string) => errors.push(message) }, '../toast': { toast() {} },
    '../loaders/api': { connectInstallQueueSSE: (next: typeof subscription) => { subscription = next; return () => {}; } }, '../store': store,
    '../content-activity': { markContentChanged() {} }, '../dto-install': installContract,
    '../dto-core': sharedStore ? coreContract : { versionsResponse: (value: unknown) => value, instancesResponse: (value: unknown) => value },
    '../install-item': source<typeof import('../../src/install-item')>('install-item.ts'),
    './download-view-models': source<typeof import('../../src/machines/download-view-models')>('machines/download-view-models.ts'),
  }, clock);
  return { machine, store, calls, errors, clock, read: (next: typeof read): void => { read = next; },
    emit(response: InstallQueueStateResponse): void { assert.ok(subscription); subscription(response); },
  };
}

test('required initial hydration joins the stream projection without a duplicate instance or version read', async () => {
  const h = downloadsHarness();
  const snapshot = queue('first', 1, 0);
  const queueReply = deferred<unknown>();
  const versionsReply = deferred<unknown>();
  h.read(async (path) => path === '/install/queue' ? queueReply.promise : path === '/versions'
    ? versionsReply.promise : { instances: ['initial'], last_instance_id: 'initial' });
  let ready = false;
  const refresh = h.machine.refreshInstallQueue({ connectActive: true, requireInstalledState: true }).then(() => { ready = true; });
  h.emit(snapshot);
  await flush();
  queueReply.resolve(snapshot);
  await flush();
  assert.equal(ready, false);
  assert.equal(h.calls.filter((path) => path === '/instances').length, 1);
  versionsReply.resolve({ versions: ['initial'] });
  await refresh;
  assert.deepEqual(h.store.instances.value, ['initial']);
  assert.equal(h.calls.filter((path) => path === '/versions').length, 1);
});

test('required hydration follows newer registry revisions and restarted epochs before reporting readiness', async () => {
  for (const next of [queue('first', 2, 1), queue('restarted', 1, 0)]) {
    const h = downloadsHarness();
    const oldVersions = deferred<unknown>();
    const newVersions = deferred<unknown>();
    h.read(async (path) => path === '/install/queue' ? queue('first', 1, 0) : path === '/versions'
      ? oldVersions.promise : { instances: ['old'], last_instance_id: 'old' });
    let ready = false;
    const refresh = h.machine.refreshInstallQueue({ requireInstalledState: true }).then(() => { ready = true; });
    await flush();
    h.read(async (path) => path === '/versions' ? newVersions.promise : { instances: ['current'], last_instance_id: 'current' });
    const current = h.machine.applyInstallQueueResponse(next);
    await flush();
    oldVersions.resolve({ versions: ['old'] });
    await flush();
    assert.equal(ready, false);
    assert.deepEqual(h.store.instances.value, []);
    newVersions.resolve({ versions: ['current'] });
    await Promise.all([refresh, current]);
    assert.deepEqual(h.store.versions.value, ['current']);
    assert.deepEqual(h.store.instances.value, ['current']);
    assert.equal(h.store.lastInstanceId.value, 'current');
    assert.equal(h.calls.filter((path) => path === '/instances').length, 2);
  }
});

test('new registry cursors drain the active read pair before reading only the latest cursor', async () => {
  for (const failure of [undefined, '/versions', '/instances']) {
    const h = downloadsHarness();
    const oldVersions = deferred<unknown>();
    const oldInstances = deferred<unknown>();
    let active = 0;
    let maximumActive = 0;
    const track = async (reply: Promise<unknown>): Promise<unknown> => {
      maximumActive = Math.max(maximumActive, ++active);
      try { return await reply; } finally { active -= 1; }
    };
    h.read((path) => track(path === '/versions' ? oldVersions.promise : oldInstances.promise));
    const old = h.machine.applyInstallQueueResponse(queue('first', 1, 0));
    await flush();
    h.read((path) => track(Promise.resolve(path === '/versions'
      ? { versions: ['latest'] } : { instances: ['latest'], last_instance_id: 'latest' })));
    const middle = h.machine.applyInstallQueueResponse(queue('first', 2, 1));
    const latest = h.machine.applyInstallQueueResponse(queue('first', 3, 2));
    await flush();
    assert.equal(h.calls.length, 2, 'new cursors must join both active requests');
    assert.deepEqual(h.store.instances.value, []);

    if (failure === '/instances') oldInstances.reject(new Error('Obsolete instance read failed'));
    else if (failure === '/versions') oldVersions.reject(new Error('Obsolete version read failed'));
    else oldVersions.resolve({ versions: ['obsolete'] });
    await flush();
    assert.equal(h.calls.length, 2, 'one settled request must not release the other active request');
    assert.deepEqual(h.store.instances.value, []);
    if (failure === '/instances') oldVersions.resolve({ versions: ['obsolete'] });
    else oldInstances.resolve({ instances: ['obsolete'], last_instance_id: 'obsolete' });
    await Promise.all([old, middle, latest]);
    assert.equal(maximumActive, 2);
    assert.equal(active, 0);
    assert.equal(h.calls.length, 4, 'the intermediate cursor must not start another read pair');
    assert.deepEqual(h.store.versions.value, ['latest']);
    assert.deepEqual(h.store.instances.value, ['latest']);
    assert.equal(h.store.lastInstanceId.value, 'latest');
    assert.equal(h.errors.length, 0);
    assert.equal(h.clock.timeouts.size, 0);
    await h.machine.applyInstallQueueResponse(queue('first', 4, 3));
    assert.equal(h.calls.length, 6, 'completed reads must release the next invalidation');
    assert.equal(active, 0);
  }
});

test('a superseded initial queue response still awaits the current stream registry projection', async () => {
  const h = downloadsHarness();
  const oldQueue = deferred<unknown>();
  const currentVersions = deferred<unknown>();
  h.read(async (path) => path === '/install/queue' ? oldQueue.promise : path === '/versions'
    ? currentVersions.promise : { instances: ['current'], last_instance_id: null });
  let ready = false;
  const refresh = h.machine.refreshInstallQueue({ connectActive: true, requireInstalledState: true }).then(() => { ready = true; });
  h.emit(queue('new', 1, 0));
  await flush();
  oldQueue.resolve(queue('old', 10, 9));
  await flush();
  assert.equal(ready, false);
  currentVersions.resolve({ versions: ['current'] });
  await refresh;
  assert.equal(h.machine.downloadQueue.value.view_model.status_label, 'new');
  assert.deepEqual(h.store.instances.value, ['current']);
  assert.equal(h.calls.filter((path) => path === '/instances').length, 1);
});

test('required hydration propagates a shared read failure and explicit retry bypasses background backoff', async () => {
  const h = downloadsHarness();
  const versionsReply = deferred<unknown>();
  h.read(async (path) => path === '/install/queue' ? queue('first', 1, 0) : path === '/versions'
    ? versionsReply.promise : { instances: ['ready'], last_instance_id: null });
  const initial = assert.rejects(h.machine.refreshInstallQueue({ requireInstalledState: true }), /Unavailable/);
  const background = h.machine.applyInstallQueueResponse(queue('first', 1, 0));
  await flush();
  versionsReply.reject(new Error('Unavailable'));
  await Promise.all([initial, background]);
  assert.deepEqual(h.store.instances.value, []);
  assert.equal(h.clock.timeouts.size, 1);
  h.read(async (path) => path === '/install/queue' ? queue('first', 1, 0) : path === '/versions'
    ? { versions: ['ready'] } : { instances: ['ready'], last_instance_id: null });
  await h.machine.refreshInstallQueue({ requireInstalledState: true });
  assert.deepEqual(h.store.instances.value, ['ready']);
  assert.equal(h.clock.timeouts.size, 0);
  assert.equal(h.calls.filter((path) => path === '/instances').length, 2);
});

test('a required attempt cannot reuse a projection read that began before the attempt at the same queue cursor', async () => {
  const h = downloadsHarness();
  const oldVersions = deferred<unknown>();
  h.read(async (path) => path === '/versions' ? oldVersions.promise : { instances: ['old'], last_instance_id: 'old' });
  const oldRead = h.machine.applyInstallQueueResponse(queue('first', 1, 0));
  await flush();
  h.read(async (path) => path === '/install/queue' ? queue('first', 1, 0) : path === '/versions'
    ? { versions: ['fresh'] } : { instances: ['fresh'], last_instance_id: 'fresh' });
  const freshRead = h.machine.refreshInstallQueue({ requireInstalledState: true });
  await flush();
  assert.equal(h.calls.filter((path) => path === '/instances').length, 1);
  oldVersions.resolve({ versions: ['old'] });
  await Promise.all([oldRead, freshRead]);
  assert.deepEqual(h.store.versions.value, ['fresh']);
  assert.deepEqual(h.store.instances.value, ['fresh']);
  assert.equal(h.store.lastInstanceId.value, 'fresh');
  assert.equal(h.calls.filter((path) => path === '/instances').length, 2);
});

test('a failed newer projection cannot be hidden by completion of an obsolete required read', async () => {
  const h = downloadsHarness();
  const oldVersions = deferred<unknown>();
  h.read(async (path) => path === '/install/queue' ? queue('first', 1, 0) : path === '/versions'
    ? oldVersions.promise : { instances: ['old'], last_instance_id: 'old' });
  const initial = assert.rejects(h.machine.refreshInstallQueue({ requireInstalledState: true }), /New projection unavailable/);
  await flush();
  h.read(async () => { throw new Error('New projection unavailable'); });
  const current = h.machine.applyInstallQueueResponse(queue('first', 2, 1));
  oldVersions.resolve({ versions: ['old'] });
  await Promise.all([initial, current]);
  assert.deepEqual(h.store.instances.value, []);
  assert.equal(h.calls.filter((path) => path === '/instances').length, 2);
  assert.equal(h.clock.timeouts.size, 1);
  h.machine.disconnectInstallQueue();
});

test('disconnecting a required projection discards its late read and does not restart it behind the caller', async () => {
  const h = downloadsHarness();
  const versionsReply = deferred<unknown>();
  h.read(async (path) => path === '/install/queue' ? queue('first', 1, 0) : path === '/versions'
    ? versionsReply.promise : { instances: ['late'], last_instance_id: 'late' });
  const stopped = assert.rejects(h.machine.refreshInstallQueue({ requireInstalledState: true }), /interrupted/);
  await flush();
  h.machine.disconnectInstallQueue();
  versionsReply.resolve({ versions: ['late'] });
  await stopped;
  assert.deepEqual(h.store.instances.value, []);
  assert.equal(h.calls.filter((path) => path === '/instances').length, 1);
});

test('a fresh required read after disconnect still drains the prior backend requests', async () => {
  const h = downloadsHarness();
  const oldVersions = deferred<unknown>();
  const oldInstances = deferred<unknown>();
  h.read(async (path) => path === '/versions' ? oldVersions.promise : oldInstances.promise);
  const oldRead = h.machine.applyInstallQueueResponse(queue('first', 1, 0));
  await flush();
  h.machine.disconnectInstallQueue();
  h.read(async (path) => path === '/install/queue' ? queue('first', 1, 0) : path === '/versions'
    ? { versions: ['fresh'] } : { instances: ['fresh'], last_instance_id: 'fresh' });
  const freshRead = h.machine.refreshInstallQueue({ requireInstalledState: true });
  await flush();
  assert.equal(h.calls.filter((path) => path === '/instances').length, 1);
  oldVersions.reject(new Error('Disconnected read failed'));
  await flush();
  assert.equal(h.calls.filter((path) => path === '/instances').length, 1);
  oldInstances.resolve({ instances: ['old'], last_instance_id: 'old' });
  await Promise.all([oldRead, freshRead]);
  assert.deepEqual(h.store.instances.value, ['fresh']);
  assert.equal(h.calls.filter((path) => path === '/instances').length, 2);
  assert.equal(h.errors.length, 0);
  assert.equal(h.clock.timeouts.size, 0);
});

test('coalesced successful installs invalidate views without an observed active phase', async () => {
  const harness = downloadsHarness();
  await harness.machine.applyInstallQueueResponse(queue('first', 1, 0));
  harness.calls.length = 0;
  await harness.machine.applyInstallQueueResponse(queue('first', 8, 0));
  assert.equal(harness.calls.length, 0);
  await harness.machine.applyInstallQueueResponse(queue('first', 25, 1));
  assert.deepEqual(harness.calls, ['/versions', '/instances']);
  assert.deepEqual(harness.store.versions.value, ['installed']);
});

test('an older downloads projection cannot overwrite readiness published after session settlement', async () => {
  const launch = launchHarness();
  const downloads = downloadsHarness(launch.store);
  downloads.read(async (path) => path === '/versions'
    ? { versions: [] } : { instances: [instance(false)], last_instance_id: 'instance-1' });
  await downloads.machine.applyInstallQueueResponse(queue('first', 1, 0));

  // Removing a waiting install can advance the registry while this game runs.
  // Its instance read finishes first; the paired versions read is still pending.
  const oldVersions = deferred<unknown>();
  downloads.read(async (path) => path === '/versions'
    ? oldVersions.promise : { instances: [instance(false)], last_instance_id: 'instance-1' });
  const removedQueue = queue('first', 25, 1);
  const oldProjection = downloads.machine.applyInstallQueueResponse(removedQueue);
  await flush();
  assert.ok(launch.store.launchSessions.value['instance-1']);

  // Drive actual terminal handling, final log drainage and its readiness read.
  launch.emit(status(2, true), 'status');
  launch.finalLogs.resolve({ entries: [] });
  await flush();
  assert.equal(launch.store.launchSessions.value['instance-1'], undefined);
  assert.equal(launch.calls.filter((path) => path === '/instances/instance-1').length, 1);
  assert.equal(launch.store.instances.value[0].launch_action.label, 'Launch');

  downloads.read(async (path) => path === '/versions'
    ? { versions: [] } : { instances: [instance(true)], last_instance_id: 'instance-1' });
  oldVersions.resolve({ versions: [] });
  await oldProjection;
  assert.equal(launch.errors.length + downloads.errors.length, 0);
  assert.equal(launch.store.launchSessions.value['instance-1'], undefined);
  assert.equal(launch.store.instances.value[0].launch_action.label, 'Launch',
    'the delayed registry response must not restore the ended session\'s Unavailable readiness');
  assert.equal(launch.store.instances.value[0].launchable, true);
  assert.equal(downloads.calls.filter((path) => path === '/instances').length, 3);
  assert.equal(downloads.clock.timeouts.size, 0);
});

test('a complete session lifetime invalidates an older projection while terminal readiness is still pending', async () => {
  const launch = launchHarness(undefined, false);
  const downloads = downloadsHarness(launch.store);
  const oldVersions = deferred<unknown>();
  const oldInstances = deferred<unknown>();
  const terminalReadiness = deferred<unknown>();
  downloads.read(async (path) => path === '/versions' ? oldVersions.promise : oldInstances.promise);
  const projection = downloads.machine.applyInstallQueueResponse(queue('first', 1, 0));
  await flush();

  launch.actions.confirmLaunch('instance-1', statusContract.launchSessionsResponse({ sessions: [status()] })['instance-1']);
  launch.launch.reconnectLaunchSession('instance-1', 'Example');
  oldInstances.resolve({ instances: [instance(false)], last_instance_id: null });
  await flush();
  launch.readInstance(() => terminalReadiness.promise);
  launch.emit(status(2, true), 'status');
  launch.finalLogs.resolve({ entries: [] });
  await flush();
  assert.equal(Object.keys(launch.store.launchSessions.value).length, 0);
  assert.equal(launch.store.launchState.value.status, 'idle');
  assert.equal(launch.calls.filter((path) => path === '/instances/instance-1').length, 1);

  downloads.read(async (path) => path === '/versions'
    ? { versions: [] } : { instances: [instance(true)], last_instance_id: null });
  oldVersions.resolve({ versions: [] });
  await projection;
  terminalReadiness.resolve(instance(true));
  await flush();
  assert.equal(launch.store.instances.value[0].launch_action.label, 'Launch');
  assert.equal(downloads.calls.filter((path) => path === '/instances').length, 2);
  assert.equal(downloads.errors.length + launch.errors.length, 0);
  assert.equal(downloads.clock.timeouts.size, 0);
});

for (const change of ['account selection', 'config', 'instance edit', 'instance removal', 'versions', 'last instance', 'preparation lifetime'] as const) {
  test(`a downloads projection rebases after a newer ${change} without advancing the queue cursor`, async () => {
    const launch = launchHarness(undefined, false);
    launch.actions.setConfig(configSnapshot());
    const downloads = downloadsHarness(launch.store);
    const oldVersions = deferred<unknown>();
    downloads.read(async (path) => path === '/versions'
      ? oldVersions.promise : { instances: [instance(false)], last_instance_id: null });
    const projection = downloads.machine.applyInstallQueueResponse(queue('first', 1, 0));
    await flush();
    if (change === 'account selection') launch.actions.setConfig({ ...configSnapshot(), account_selection_revision: 2 });
    if (change === 'config') launch.actions.setConfig({ ...configSnapshot(), revision: 2, max_memory_mb: 8192 });
    if (change === 'instance edit') launch.actions.updateInstanceInList({ ...instance(true), name: 'Renamed' });
    if (change === 'instance removal') launch.actions.removeInstance('instance-1');
    if (change === 'versions') launch.store.versions.value = [];
    if (change === 'last instance') launch.store.lastInstanceId.value = 'instance-1';
    if (change === 'preparation lifetime') { launch.actions.startLaunch('instance-1'); launch.actions.endLaunchPrep(); }
    const expected = change === 'instance removal' ? [] : [{ ...instance(true), name: change === 'instance edit' ? 'Renamed' : 'Example' }];
    const lastInstance = launch.store.lastInstanceId.value;
    downloads.read(async (path) => path === '/versions'
      ? { versions: [] } : { instances: expected, last_instance_id: lastInstance });
    oldVersions.resolve({ versions: [] });
    await projection;
    assert.deepEqual(JSON.parse(JSON.stringify(launch.store.instances.value)), expected);
    assert.equal(launch.store.lastInstanceId.value, lastInstance);
    assert.equal(launch.store.config.value?.account_selection_revision, change === 'account selection' ? 2 : 1);
    assert.equal(downloads.calls.filter((path) => path === '/instances').length, 2);
    assert.equal(downloads.errors.length, 0);
    await downloads.machine.applyInstallQueueResponse(queue('first', 2, 0));
    assert.equal(downloads.calls.filter((path) => path === '/instances').length, 2, 'the rebased cursor is reconciled');
  });
}

test('same-session status ticks, same-owner preparation progress and navigation do not starve registry reads', async () => {
  const launch = launchHarness();
  const downloads = downloadsHarness(launch.store);
  launch.actions.startLaunch('instance-2');
  const versionsReply = deferred<unknown>();
  downloads.read(async (path) => path === '/versions'
    ? versionsReply.promise : { instances: [instance(false)], last_instance_id: null });
  const projection = downloads.machine.applyInstallQueueResponse(queue('first', 1, 0));
  await flush();
  for (let revision = 2; revision <= 4; revision++) {
    launch.emit(status(revision), 'status');
    launch.actions.updateLaunchPrep('instance-2', revision * 10, 'Preparing');
  }
  launch.actions.selectInstance(null);
  versionsReply.resolve({ versions: [] });
  await projection;
  assert.equal(downloads.calls.filter((path) => path === '/instances').length, 1);
  assert.equal(launch.store.launchSessions.value['instance-1'].statusRevision, 4);
  assert.equal(launch.store.selectedInstanceId.value, null);
  assert.equal(downloads.clock.timeouts.size, 0);
});

test('an obsolete read failure rebases silently and required hydration awaits the fresh projection', async () => {
  const launch = launchHarness(undefined, false);
  const downloads = downloadsHarness(launch.store);
  const oldVersions = deferred<unknown>();
  const freshVersions = deferred<unknown>();
  downloads.read(async (path) => path === '/install/queue' ? queue('first', 1, 0) : path === '/versions'
    ? oldVersions.promise : { instances: [instance(false)], last_instance_id: null });
  let ready = false;
  const initial = downloads.machine.refreshInstallQueue({ requireInstalledState: true }).then(() => { ready = true; });
  await flush();
  launch.actions.setConfig(configSnapshot());
  downloads.read(async (path) => path === '/versions'
    ? freshVersions.promise : { instances: [instance(true)], last_instance_id: null });
  oldVersions.reject(new Error('Obsolete network failure'));
  await flush();
  assert.equal(ready, false);
  assert.equal(downloads.calls.filter((path) => path === '/instances').length, 2);
  assert.equal(downloads.errors.length, 0);
  freshVersions.resolve({ versions: [] });
  await initial;
  assert.equal(launch.store.instances.value[0].launch_action.label, 'Launch');
  assert.equal(downloads.clock.timeouts.size, 0);
});

test('sustained supersession fails required hydration truthfully and retries the unreconciled cursor on a bounded timer', async () => {
  const launch = launchHarness(undefined, false);
  const downloads = downloadsHarness(launch.store);
  const replies = [deferred<unknown>(), deferred<unknown>()];
  let attempt = 0;
  downloads.read(async (path) => path === '/install/queue' ? queue('first', 1, 0) : path === '/versions'
    ? replies[attempt++].promise : { instances: [instance(false)], last_instance_id: null });
  const failed = assert.rejects(downloads.machine.refreshInstallQueue({ requireInstalledState: true }), /state changed while refreshing/);
  for (const [index, reply] of replies.entries()) {
    await flush();
    launch.actions.setConfig({ ...configSnapshot(), account_selection_revision: index + 1 });
    reply.resolve({ versions: [] });
  }
  await failed;
  assert.equal(downloads.calls.filter((path) => path === '/instances').length, 2);
  assert.equal(downloads.errors.length, 0);
  assert.equal(downloads.clock.timeouts.size, 1);
  assert.equal([...downloads.clock.timeouts.values()][0].delay, 500);
  await downloads.machine.applyInstallQueueResponse(queue('first', 2, 0));
  assert.equal(downloads.calls.filter((path) => path === '/instances').length, 2);
  downloads.read(async (path) => path === '/versions'
    ? { versions: [] } : { instances: [instance(true)], last_instance_id: null });
  downloads.clock.runTimeout();
  await flush();
  assert.equal(launch.store.instances.value[0].launch_action.label, 'Launch');
  assert.equal(downloads.calls.filter((path) => path === '/instances').length, 3);
  assert.equal(downloads.clock.timeouts.size, 0);
});

test('disconnect during a rebase prevents late publication and cannot schedule a retry', async () => {
  const launch = launchHarness(undefined, false);
  const downloads = downloadsHarness(launch.store);
  const first = deferred<unknown>();
  const second = deferred<unknown>();
  downloads.read(async (path) => path === '/versions' ? first.promise : { instances: [instance(false)], last_instance_id: null });
  const projection = downloads.machine.applyInstallQueueResponse(queue('first', 1, 0));
  await flush();
  launch.actions.setConfig(configSnapshot());
  downloads.read(async (path) => path === '/versions' ? second.promise : { instances: [instance(true)], last_instance_id: null });
  first.resolve({ versions: [] });
  await flush();
  downloads.machine.disconnectInstallQueue();
  const current = launch.store.instances.value;
  launch.actions.startLaunch('instance-1');
  launch.actions.endLaunchPrep();
  second.resolve({ versions: [] });
  await projection;
  assert.equal(launch.store.instances.value, current);
  assert.equal(downloads.calls.filter((path) => path === '/instances').length, 2);
  assert.equal(downloads.clock.timeouts.size, 0);
  assert.equal(downloads.errors.length, 0);
});

test('queue restart accepts lower revisions and rejects delayed snapshots from its retired process', async () => {
  const harness = downloadsHarness();
  await harness.machine.applyInstallQueueResponse(queue('old', 80, 9));
  harness.calls.length = 0;
  await harness.machine.applyInstallQueueResponse(queue('new', 1, 0));
  assert.equal(harness.machine.downloadQueue.value.view_model.status_label, 'new');
  assert.deepEqual(harness.calls, ['/versions', '/instances']);
  harness.calls.length = 0;
  await harness.machine.applyInstallQueueResponse(queue('old', 100, 10));
  assert.equal(harness.machine.downloadQueue.value.view_model.status_label, 'new');
  assert.equal(harness.calls.length, 0);
});

test('a late initial queue read cannot replace the restarted stream snapshot', async () => {
  const harness = downloadsHarness();
  const initial = deferred<unknown>();
  harness.read(async (path) => path === '/install/queue' ? initial.promise
    : path === '/versions' ? { versions: [] } : { instances: [], last_instance_id: null });
  const refresh = harness.machine.refreshInstallQueue();
  await harness.machine.applyInstallQueueResponse(queue('new', 1, 0));
  initial.resolve(queue('old', 80, 9));
  await refresh;
  assert.equal(harness.machine.downloadQueue.value.view_model.status_label, 'new');
});

test('delayed registry reads from an old process cannot overwrite the restarted process views', async () => {
  const harness = downloadsHarness();
  const oldVersions = deferred<unknown>();
  harness.read(async (path) => path === '/versions' ? oldVersions.promise
    : { instances: ['old'], last_instance_id: 'old' });
  const oldRefresh = harness.machine.applyInstallQueueResponse(queue('old', 80, 9));
  await flush();
  harness.read(async (path) => path === '/versions' ? { versions: ['new'] }
    : { instances: ['new'], last_instance_id: 'new' });
  const current = harness.machine.applyInstallQueueResponse(queue('new', 1, 0));
  oldVersions.resolve({ versions: ['old'] });
  await Promise.all([oldRefresh, current]);
  assert.deepEqual(harness.store.versions.value, ['new']);
  assert.deepEqual(harness.store.instances.value, ['new']);
  assert.equal(harness.store.lastInstanceId.value, 'new');
});

test('failed registry reads retry without a new queue event and cancellation clears the retry', async () => {
  const harness = downloadsHarness();
  let fail = true;
  harness.read(async (path) => {
    if (fail) throw new Error('Unavailable');
    return path === '/versions' ? { versions: ['recovered'] } : { instances: [], last_instance_id: null };
  });
  await harness.machine.applyInstallQueueResponse(queue('first', 1, 0));
  assert.equal(harness.errors.length, 1);
  assert.equal(harness.clock.timeouts.size, 1);
  fail = false;
  harness.clock.runTimeout();
  await flush();
  assert.deepEqual(harness.store.versions.value, ['recovered']);
  fail = true;
  await harness.machine.applyInstallQueueResponse(queue('first', 2, 1));
  harness.machine.disconnectInstallQueue();
  assert.equal(harness.clock.timeouts.size, 0);
});

test('failure dismissal is scoped to an operation and cannot hide the next failed install', async () => {
  const harness = downloadsHarness();
  const failure = () => harness.machine.downloadFailure.value;
  const first = queue('first', 1, 0);
  first.latest_failure = {
    failed_at_ms: 100, queue_id: 'queued-1', install_id: 'install-1', operation_id: 'operation-1',
    label: 'Minecraft', install_item: { version_id: '1.21.6' },
    failure_view_model: { state_id: 'failed', title: 'Install failed', tone: 'err', summary: 'Download interrupted', details: [],
      retry_action: { action: 'retry', label: 'Retry install', enabled: true },
      dismiss_action: { action: 'dismiss', label: 'Dismiss', enabled: true } },
  };
  await harness.machine.applyInstallQueueResponse(first);
  assert.equal(failure()?.displayName, 'Minecraft');
  harness.machine.clearDownloadFailure();
  assert.equal(failure(), null);
  await harness.machine.applyInstallQueueResponse({ ...first, revision: 2 });
  assert.equal(failure(), null);
  await harness.machine.applyInstallQueueResponse({ ...first, revision: 3,
    latest_failure: { ...first.latest_failure, operation_id: 'operation-2' } });
  assert.equal(failure()?.displayName, 'Minecraft');
  await harness.machine.applyInstallQueueResponse(queue('first', 4, 0));
  assert.equal(failure(), null);
});

test('new event connections deliver their complete snapshot even when revisions restart', async () => {
  const clock = timers();
  const sources: FakeEventSource[] = [];
  const seen: number[] = [];
  class FakeEventSource {
    readonly listeners = new Map<string, (event: { data: string }) => void>();
    onopen?: () => void;
    onerror?: () => void;
    constructor() { sources.push(this); }
    addEventListener(name: string, callback: (event: { data: string }) => void): void { this.listeners.set(name, callback); }
    close(): void {}
    emit(revision: number): void { this.listeners.get('message')?.({ data: JSON.stringify({ revision, value: revision }) }); }
  }
  const events = source<typeof import('../../src/backend/events')>('backend/events.ts', {
    '../api': { apiEventSourceUrl: async () => '/fresh-ticket' },
  }, { ...clock, EventSource: FakeEventSource });
  const close = events.subscribeApiEvents('/install/queue/events', {
    decode: (value: unknown) => { assert.equal(typeof value, 'number'); return value as number; },
    onValue: (value) => seen.push(value),
  });
  await flush();
  sources[0].emit(80); sources[0].emit(79); sources[0].onerror?.();
  clock.runTimeout();
  await flush();
  sources[0].emit(100);
  sources[1].emit(1); sources[1].emit(1);
  assert.deepEqual(seen, [80, 1]);
  close();
});

test('Downloads renders loader labels and Minecraft versions while retaining queue identity', async () => {
  const h = downloadsHarness();
  const snapshot = queue('first', 1, 0);
  const item = {
    version_id: 'loader-v2-opaque-installed-identity',
    loader: { component_id: 'net.fabricmc.fabric-loader', build_id: 'opaque-build-identity',
      minecraft_version: '1.20.1', loader_version: '0.19.5' },
  } as const;
  const label = 'Fabric 0.19.5 for Minecraft 1.20.1';
  snapshot.active = {
    queue_id: 'active-queue', kind: 'loader', title: label, label, summary: 'Downloading files',
    install_item: item,
    progress: { phase_id: 'download', label: 'Downloading files', progress_pct: 25, terminal: false, failed: false },
  };
  snapshot.items = [{
    queue_id: 'next-queue', state_id: 'queued', kind: 'loader', title: label, label,
    summary: 'Waiting to install', detail: '', position: 1, total: 1, install_item: item,
    remove_action: { action: 'remove_from_queue', label: 'Remove from queue', enabled: true },
  }];
  snapshot.view_model.queued_count = 1;
  await h.machine.applyInstallQueueResponse(snapshot);
  assert.equal(h.machine.activeDownload.value?.item.versionId, item.version_id);
  const removed: string[] = [];
  const imports: Record<string, unknown> = {
    'preact/jsx-runtime': requireDependency('preact/jsx-runtime'),
    '../../ui/Atoms': { IconButton: 'IconButton', Meter: 'Meter' },
    '../../ui/Icons': { Icon: 'Icon' },
    '../../ui/DownloadFailureNotice': { DownloadFailureNotice: 'DownloadFailureNotice' },
    '../../hooks/use-now': { useNowTicker: () => 0 },
    '../../machines/downloads': { ...h.machine, removeQueuedInstall: (id: string) => removed.push(id) },
  };
  const filename = resolve(frontend, 'src/views/downloads/DownloadsView.tsx');
  const compiled = ts.transpileModule(readFileSync(filename, 'utf8'), {
    fileName: filename,
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020,
      jsx: ts.JsxEmit.ReactJSX, jsxImportSource: 'preact' },
  });
  const exports = {};
  vm.runInNewContext(compiled.outputText, { exports, require(id: string): unknown {
    if (Object.prototype.hasOwnProperty.call(imports, id)) return imports[id];
    throw new Error(`Unreviewed downloads view dependency: ${id}`);
  } }, { filename });
  const { DownloadsView } = exports as typeof import('../../src/views/downloads/DownloadsView');
  type Node = { type: unknown; props: Record<string, unknown> };
  function nodes(value: unknown): Node[] {
    if (Array.isArray(value)) return value.flatMap(nodes);
    if (!value || typeof value !== 'object' || !('props' in value)) return [];
    const node = value as Node;
    return [node, ...nodes(node.props.children)];
  }
  const rendered = nodes(DownloadsView());
  assert.equal(rendered.find((node) => node.type === 'h2')?.props.children, label);
  assert.equal(rendered.find((node) => node.props.class === 'cp-dl-queue-label')?.props.children, label);
  assert.equal(rendered.find((node) => node.props.class === 'cp-dl-queue-version')?.props.children, '1.20.1');
  assert.equal(rendered.some((node) => node.props.children === item.version_id), false);
  const remove = rendered.find((node) => node.type === 'IconButton');
  assert.ok(remove);
  (remove.props.onClick as () => void)();
  assert.deepEqual(removed, ['next-queue']);
});
