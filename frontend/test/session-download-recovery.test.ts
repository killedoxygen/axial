import assert from 'node:assert/strict';
import test from 'node:test';
import { apiModule, isApiError, loadSource as source, requireDependency, type Call } from './source';
import { configView, enrichedInstance, queueSnapshot } from './wire';
import type { ApiEventOptions } from '../src/platform/events';
import type { ConfigView } from '../src/generated/ConfigView';
import type { EnrichedInstance } from '../src/generated/EnrichedInstance';
import type { InstallFailureViewModel } from '../src/generated/InstallFailureViewModel';
import type { InstallQueueFailureViewModel } from '../src/generated/InstallQueueFailureViewModel';
import type { InstallQueueStateResponse } from '../src/generated/InstallQueueStateResponse';
import type { SessionSnapshot } from '../src/generated/SessionSnapshot';

type Subscribe = Parameters<typeof apiModule>[2];

function timers() {
  let next = 0;
  const timeouts = new Map<number, { callback: () => void; delay: number }>();
  const intervals = new Map<number, () => void>();
  return {
    timeouts,
    intervals,
    setTimeout(callback: () => void, delay: number): number {
      const id = ++next;
      timeouts.set(id, { callback, delay });
      return id;
    },
    clearTimeout(id: number): void {
      timeouts.delete(id);
    },
    setInterval(callback: () => void): number {
      const id = ++next;
      intervals.set(id, callback);
      return id;
    },
    clearInterval(id: number): void {
      intervals.delete(id);
    },
    runTimeout(): void {
      const timer = timeouts.entries().next().value;
      assert.ok(timer, 'expected a pending timeout');
      timeouts.delete(timer[0]);
      timer[1].callback();
    },
  };
}

function deferred<T>() {
  let resolveValue!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolveValue = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve: resolveValue, reject };
}

const flush = (): Promise<void> => new Promise((done) => setImmediate(done));

/** An error response as the transport raises it. */
function refusal(status: number, payload: { error?: string; code?: string }): Error {
  return Object.assign(new Error(payload.error ?? `Request failed (${status})`), { name: 'ApiError', status, payload });
}

const unknownSession = { error: 'The instance was not found.', code: 'instance_not_found' };

function status(revision = 1, terminal = false): SessionSnapshot {
  return {
    session_id: 'session-1',
    instance_id: 'instance-1',
    revision,
    phase: terminal ? 'exited' : 'running',
    launched_at: '2026-09-08T08:00:00Z',
    started_at_ms: null,
    pid: null,
    process_alive: !terminal,
    stop_allowed: !terminal,
    exit_code: null,
    tree_settled: terminal,
    output_drained: terminal,
    boot_observed: true,
    notice: null,
    outcome: terminal ? { kind: 'clean', reason: 'clean_exit', failure_class: null, summary: 'Game closed' } : null,
    view_model: {
      state_id: terminal ? 'exited' : 'running',
      label: terminal ? 'Ended' : 'Playing',
      progress_pct: 100,
      terminal,
      playing: !terminal,
      process_live: !terminal,
      can_stop: !terminal,
    },
  };
}

function log(sequence: number) {
  return { sequence, source: 'stdout', text: `line-${sequence}`, truncated: false };
}

function instance(launchable = false): EnrichedInstance {
  return enrichedInstance({
    id: 'instance-1',
    name: 'Example',
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

function configSnapshot(): ConfigView {
  return configView({ performance_mode: 'vanilla', theme: 'obsidian', music_enabled: false, music_volume: 50 });
}

/** The state owners one window holds, shared by the launch and downloads controllers under test. */
function stateOwners() {
  const launchState = source<typeof import('../src/launch/state')>('launch/state.ts');
  const instanceState = source<typeof import('../src/instances/state')>('instances/state.ts');
  const settings = source<typeof import('../src/settings/state')>('settings/state.ts');
  return { launchState, instanceState, settings, store: { ...launchState, ...instanceState, config: settings.config } };
}

function launchHarness(intentResult?: unknown, running = true) {
  const owners = stateOwners();
  const { launchState, instanceState, settings, store } = owners;
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
  instanceState.instances.value = [instance(true)];
  instanceState.selectedInstanceId.value = 'instance-1';
  if (intentResult === undefined && running)
    launchState.launchSessions.value = launchState.liveSessions([currentStatus]);
  const call: Call = async (method, path, body) => {
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
  };
  const subscribe: Subscribe = (_path, options) => {
    connections++;
    subscription = options as ApiEventOptions<unknown>;
    return () => {
      closed++;
    };
  };
  const instancesApi = apiModule<typeof import('../src/instances/api')>('instances/api.ts', call);
  const readiness = source<typeof import('../src/instances/readiness')>(
    'instances/readiness.ts',
    {
      'instances/api': instancesApi,
      'settings/state': settings,
      'instances/state': instanceState,
      'launch/state': launchState,
      'app/toast': { showError: (message: string) => errors.push(message) },
    },
    { window: clock },
  );
  const launch = source<typeof import('../src/launch/controller')>(
    'launch/controller.ts',
    {
      'platform/transport': { isApiError },
      'app/sound': { Sound: { init() {}, ui: (sound: string) => sounds.push(sound) } },
      'music/music': { Music: { suppress() {}, unsuppress() {} } },
      'app/log': { appendLog: (source: string, text: string) => lines.push({ source, text }) },
      'app/toast': { showError: (message: string) => errors.push(message), errMessage: String },
      'settings/state': settings,
      'instances/state': instanceState,
      'instances/api': instancesApi,
      'instances/readiness': readiness,
      'launch/state': launchState,
      'launch/notices': source('launch/notices.ts'),
      'launch/api': apiModule('launch/api.ts', call, subscribe),
    },
    { ...clock, window: clock, crypto: { randomUUID: () => '9eb6ce58-c29a-44bc-a290-aea8973e9bdb' }, Date },
  );
  if (intentResult === undefined && running) launch.reconnectLaunchSession('instance-1', 'Example');
  return {
    owners,
    store,
    actions: { ...launchState, ...instanceState, setConfig: settings.setConfig },
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

function liveSession(snapshot: SessionSnapshot, h: ReturnType<typeof launchHarness>) {
  const session = h.owners.launchState.liveSessions([snapshot])['instance-1'];
  assert.ok(session);
  return session;
}

test('bootstrap reconnect of a Playing session projects recency once without replaying launch sound', async () => {
  {
    const h = launchHarness(undefined, false);
    h.store.instances.value = [{ ...instance(), name: 'Current name', java_path: '/fixture/java' }];
    h.actions.confirmLaunch('instance-1', liveSession(status(), h));
    h.launch.reconnectLaunchSession('instance-1', 'Example');
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
    assert.equal(
      h.calls.some((path) => path.startsWith('/instances')),
      false,
    );
  }
});

for (const change of [
  'settings edit',
  'newer recency',
  'replacement session',
  'session lifetime',
  'instance removal',
]) {
  test(`first Playing recency respects current ownership after ${change}`, async () => {
    const h = launchHarness(undefined, false);
    const starting = {
      ...status(),
      view_model: { ...status().view_model, state_id: 'starting', label: 'Starting', playing: false },
    };
    h.readStatus(async () => starting);
    h.actions.confirmLaunch('instance-1', liveSession(starting, h));
    h.launch.reconnectLaunchSession('instance-1', 'Example');
    await flush();
    assert.equal(h.store.instances.value[0].last_played_at, '');
    if (change === 'settings edit') {
      h.actions.updateInstanceInList({ ...instance(), name: 'Renamed while starting', java_path: '/fixture/new-java' });
    } else if (change === 'newer recency') {
      h.actions.updateInstanceInList({ ...instance(), last_played_at: '2026-10-04T08:00:00Z' });
    } else if (change === 'instance removal') h.store.instances.value = [];
    else {
      h.actions.confirmLaunch('instance-1', liveSession({ ...status(), session_id: 'session-2' }, h));
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
  h.readStatus(async () => {
    throw refusal(404, unknownSession);
  });
  h.readIntent(async () => settledIntent());
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
  assert.ok(h.calls.includes(path));
  h.readStatus(async () => {
    throw refusal(404, unknownSession);
  });
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
  [503, { error: 'Status unavailable', code: 'instance_not_found' }],
] as const) {
  test(`unclassified or unavailable status cannot authorize an intent read: ${code} ${JSON.stringify(payload)}`, async () => {
    const h = await acceptedPlay();
    const sessions = h.store.launchSessions.value;
    h.readStatus(async () => {
      throw refusal(code, payload);
    });
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
    h.readIntent(async () => structuredClone(payload));
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
  assert.equal(h.store.selectedInstance.value?.id, other.id);
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

test('delayed live status or Stop outcomes cannot undo verified settlement while final logs are pending', async () => {
  for (const stale of ['live status', 'Stop response', 'Stop refused', 'Stop response lost'] as const) {
    const h = await acceptedPlay();
    const reply = deferred<unknown>();
    h.postKill(() => reply.promise);
    const stopping = stale === 'live status' ? undefined : h.launch.killGame();
    const kills = (): number => h.calls.filter((path) => path === `/launch/${h.sessionId}/kill`).length;
    assert.equal(kills(), stopping ? 1 : 0, stale);
    h.poll();
    await flush();
    await flush();
    const settled = h.store.launchSessions.value['instance-1'];
    assert.equal(settled.viewModel.terminal, true, stale);
    assert.equal(h.calls.filter((path) => path.endsWith('/logs')).length, 1, stale);
    assert.equal(h.closed(), 0, stale);

    if (stale === 'live status') h.emit({ ...h.accepted, revision: 8 }, 'status');
    else if (stale === 'Stop response') {
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
    } else if (stale === 'Stop refused') {
      reply.reject(refusal(404, unknownSession));
    } else reply.reject(new Error('Stop response lost'));
    await stopping;
    const current = h.store.launchSessions.value['instance-1'];
    if (stale === 'Stop refused' || stale === 'Stop response lost') assert.equal(current, settled, stale);
    assert.equal(
      current.viewModel.terminal,
      true,
      `${stale}: a delayed reply cannot undo authenticated terminal settlement`,
    );
    assert.equal(current.viewModel.playing, false, stale);
    assert.equal(current.viewModel.can_stop, false, stale);
    assert.equal(current.viewModel.process_live, false, stale);
    assert.equal(current.statusRevision, 7, stale);
    assert.deepEqual(h.errors, [], stale);
    assert.equal(kills(), stopping ? 1 : 0, stale);
    assert.equal(h.calls.filter((path) => path.endsWith('/logs')).length, 1, stale);
    h.finalLogs.resolve({ entries: [] });
    await flush();
    assert.equal(h.store.launchSessions.value['instance-1'], undefined, stale);
    assert.equal(h.closed(), 1, stale);
    assert.equal(h.calls.filter((path) => path.endsWith('/logs')).length, 1, stale);
    assert.equal(h.lines.filter((line) => line.text === 'The game was stopped from the launcher.').length, 1, stale);
    assert.deepEqual(h.sounds, ['launchSuccess'], stale);
  }
});

test('lost or unclassified Stop responses reconcile terminal status without repeating Stop', async () => {
  for (const error of [new Error('Stop response lost'), refusal(409, { error: 'Unknown outcome' })]) {
    const h = launchHarness();
    await flush();
    h.postKill(async () => {
      throw error;
    });
    let reads = 0;
    h.readStatus(async () => {
      reads++;
      return status(6, true);
    });
    await h.launch.killGame();
    assert.equal(reads, 1);
    assert.equal(h.calls.filter((path) => path.endsWith('/kill')).length, 1);
    assert.equal(h.store.launchSessions.value['instance-1'].viewModel.terminal, true);
    assert.deepEqual(h.errors, []);
    h.finalLogs.resolve({ entries: [] });
    await flush();
    assert.equal(h.store.launchSessions.value['instance-1'], undefined);
  }
});

test('unreachable Stop status stays unknown and existing status tracking can later settle it', async () => {
  const h = launchHarness();
  await flush();
  h.postKill(async () => {
    throw new Error('Stop response lost');
  });
  h.readStatus(async () => {
    throw new Error('Read unavailable');
  });
  await h.launch.killGame();
  assert.equal(h.store.launchSessions.value['instance-1'].viewModel.terminal, false);
  assert.equal(h.store.launchSessions.value['instance-1'].stopping, false);
  assert.deepEqual(h.errors, ['The stop response was interrupted. Game status is unavailable.']);
  assert.equal(h.calls.filter((path) => path.endsWith('/kill')).length, 1);
  h.readStatus(async () => status(6, true));
  h.poll();
  await flush();
  h.finalLogs.resolve({ entries: [] });
  await flush();
  assert.equal(h.store.launchSessions.value['instance-1'], undefined);
  assert.equal(h.calls.filter((path) => path.endsWith('/kill')).length, 1);
});

test('delayed Stop reconciliation cannot change a replacement or an already-settling session', async () => {
  for (const change of ['replacement', 'settlement'] as const) {
    const h = launchHarness();
    await flush();
    const reply = deferred<unknown>();
    h.postKill(async () => {
      throw new Error('Stop response lost');
    });
    h.readStatus(() => reply.promise);
    const stopping = h.launch.killGame();
    await flush();
    if (change === 'replacement') {
      h.actions.confirmLaunch('instance-1', {
        sessionId: 'session-2',
        launchedAt: '2026-09-08T08:00:01Z',
        statusRevision: 1,
        viewModel: status().view_model,
      });
    } else h.emit(status(7, true), 'status');
    const current = h.store.launchSessions.value['instance-1'];
    reply.resolve(status(6, true));
    await stopping;
    assert.equal(h.store.launchSessions.value['instance-1'], current);
    assert.deepEqual(h.errors, []);
    assert.equal(h.calls.filter((path) => path.endsWith('/kill')).length, 1);
    h.finalLogs.resolve({ entries: [] });
    await flush();
  }
});

test('the Stop owner refusal is surfaced without treating its HTTP status as an unknown mutation', async () => {
  const h = launchHarness();
  await flush();
  h.postKill(async () => {
    throw refusal(404, unknownSession);
  });
  let reads = 0;
  h.readStatus(async () => {
    reads++;
    return status(6, true);
  });
  await h.launch.killGame();
  assert.equal(reads, 0);
  assert.equal(h.store.launchSessions.value['instance-1'].stopping, false);
  assert.equal(h.store.launchSessions.value['instance-1'].viewModel.terminal, false);
  assert.equal(h.calls.filter((path) => path.endsWith('/kill')).length, 1);
  assert.equal(h.errors.length, 1);
  assert.match(h.errors[0], /Stop request was refused/);
});

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

test('an interrupted durable launch stops recovery polling and prevents a fresh launch', async () => {
  const harness = launchHarness({
    state: 'interrupted',
    session_id: 'session-1',
    error: 'Its process outcome is unknown.',
  });
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

test('a terminal status without its outcome or with an unsafe revision cannot end the session', async () => {
  for (const snapshot of [
    { ...status(2, true), outcome: null },
    { ...status(2, true), revision: Number.MAX_SAFE_INTEGER + 1 },
  ]) {
    const h = launchHarness();
    await flush();
    const sessions = h.store.launchSessions.value;
    h.emit(snapshot, 'status');
    await flush();
    assert.equal(h.store.launchSessions.value, sessions);
    assert.equal(
      h.calls.some((path) => path.endsWith('/logs')),
      false,
    );
    assert.equal(h.closed(), 0);
  }
});

test('replayed launch history is appended once per session sequence', async () => {
  const harness = launchHarness();
  await flush();
  for (const sequence of [1, 2, 1, 2, 3]) harness.emit(log(sequence), 'log');
  assert.deepEqual(
    harness.lines.map((line) => line.text),
    ['line-1', 'line-2', 'line-3'],
  );
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
  assert.deepEqual(
    harness.lines.map((line) => line.text),
    ['line-1', 'line-2', 'line-3', 'Game closed'],
  );
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
  harness.store.launchSessions.value = {
    'instance-1': {
      sessionId: 'session-2',
      launchedAt: status().launched_at,
      statusRevision: 1,
      viewModel: status().view_model,
    },
  };
  const count = harness.lines.length;
  harness.finalLogs.resolve({ entries: [log(1)] });
  await flush();
  assert.equal(harness.lines.length, count);
  assert.equal(harness.store.launchSessions.value['instance-1'].sessionId, 'session-2');
});

test('terminal readiness rechecks once after the launch reservation is released and settles truthfully', async () => {
  for (const { label, read, launchable, errors } of [
    { label: 'released reservation', read: (reads: number) => instance(reads > 1), launchable: true, errors: 0 },
    { label: 'backend refusal', read: () => instance(), launchable: false, errors: 0 },
    {
      label: 'read failure',
      read: (): EnrichedInstance => {
        throw new Error('Offline');
      },
      launchable: false,
      errors: 1,
    },
  ]) {
    const harness = launchHarness();
    harness.store.instances.value = [instance()];
    let reads = 0;
    harness.readInstance(async () => read(++reads));
    harness.emit(status(2, true), 'status');
    harness.finalLogs.resolve({ entries: [] });
    await flush();
    assert.equal(harness.store.launchSessions.value['instance-1'], undefined, label);
    assert.equal(harness.store.instances.value[0].launch_action.label, 'Unavailable', label);
    assert.equal([...harness.clock.timeouts.values()][0].delay, 250, label);
    harness.clock.runTimeout();
    await flush();
    assert.equal(harness.store.instances.value[0].launch_action.launchable, launchable, label);
    assert.equal(harness.store.instances.value[0].launch_action.label, launchable ? 'Launch' : 'Unavailable', label);
    assert.equal(reads, 2, label);
    assert.equal(harness.calls.filter((path) => path === '/instances/instance-1').length, 2, label);
    assert.equal(harness.closed(), 1, label);
    assert.equal(harness.errors.length, errors, label);
    for (const error of harness.errors) assert.match(error, /Could not refresh launch availability/, label);
    assert.equal(harness.clock.timeouts.size, 0, label);
  }
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
    if (change === 'config') harness.store.config.value = { ...harness.store.config.value } as ConfigView;
    if (change === 'session')
      harness.store.launchSessions.value = {
        'instance-1': {
          sessionId: 'session-2',
          launchedAt: status().launched_at,
          statusRevision: 1,
          viewModel: status().view_model,
        },
      };
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
  const snapshot = queueSnapshot({ queue_epoch: epoch, revision, registry_revision: registryRevision, failures: [] });
  snapshot.view_model.status_label = epoch;
  return snapshot;
}

function downloadsHarness(shared?: ReturnType<typeof launchHarness>) {
  const owners = shared?.owners ?? stateOwners();
  const clock = timers();
  const calls: string[] = [];
  const errors: string[] = [];
  let subscription: ((response: InstallQueueStateResponse) => void) | undefined;
  let read: (path: string) => Promise<unknown> = async (path) =>
    path === '/versions' ? { versions: ['installed'] } : { instances: ['ready'], last_instance_id: null };
  const call: Call = async (method, path) => {
    assert.equal(method, 'GET');
    calls.push(path);
    return read(path);
  };
  const subscribe: Subscribe = (_path, options) => {
    subscription = (response) => options.onValue(response, 'message', response.revision);
    return () => {};
  };
  const machine = source<typeof import('../src/install/queue')>(
    'install/queue.ts',
    {
      'app/toast': { errMessage: String, showError: (message: string) => errors.push(message), toast() {} },
      'install/api': apiModule('install/api.ts', call, subscribe),
      'instances/api': apiModule('instances/api.ts', call),
      'settings/state': owners.settings,
      'instances/state': owners.instanceState,
      'launch/state': owners.launchState,
      'content/activity': { markContentChanged() {} },
      'install/item': source('install/item.ts'),
      'install/presenters': source('install/presenters.ts'),
    },
    clock,
  );
  return {
    machine,
    store: owners.store,
    calls,
    errors,
    clock,
    read: (next: typeof read): void => {
      read = next;
    },
    emit(response: InstallQueueStateResponse): void {
      assert.ok(subscription);
      subscription(response);
    },
  };
}

test('required initial hydration joins the current stream projection without a duplicate instance or version read', async () => {
  for (const initial of ['matching', 'superseded'] as const) {
    const h = downloadsHarness();
    const snapshot = queue('current', 1, 0);
    const queueReply = deferred<unknown>();
    const versionsReply = deferred<unknown>();
    h.read(async (path) =>
      path === '/install/queue'
        ? queueReply.promise
        : path === '/versions'
          ? versionsReply.promise
          : { instances: ['current'], last_instance_id: null },
    );
    let ready = false;
    const refresh = h.machine.refreshInstallQueue({ connectActive: true, requireInstalledState: true }).then(() => {
      ready = true;
    });
    h.emit(snapshot);
    await flush();
    queueReply.resolve(initial === 'matching' ? snapshot : queue('old', 10, 9));
    await flush();
    assert.equal(ready, false, initial);
    assert.equal(h.calls.filter((path) => path === '/instances').length, 1, initial);
    versionsReply.resolve({ versions: ['current'] });
    await refresh;
    assert.equal(h.machine.downloadQueue.value.view_model.status_label, 'current', initial);
    assert.deepEqual(h.store.versions.value, ['current'], initial);
    assert.deepEqual(h.store.instances.value, ['current'], initial);
    assert.equal(h.calls.filter((path) => path === '/instances').length, 1, initial);
    assert.equal(h.calls.filter((path) => path === '/versions').length, 1, initial);
  }
});

test('required hydration follows newer registry revisions and restarted epochs before reporting readiness', async () => {
  for (const next of [queue('first', 2, 1), queue('restarted', 1, 0)]) {
    const h = downloadsHarness();
    const oldVersions = deferred<unknown>();
    const newVersions = deferred<unknown>();
    h.read(async (path) =>
      path === '/install/queue'
        ? queue('first', 1, 0)
        : path === '/versions'
          ? oldVersions.promise
          : { instances: ['old'], last_instance_id: 'old' },
    );
    let ready = false;
    const refresh = h.machine.refreshInstallQueue({ requireInstalledState: true }).then(() => {
      ready = true;
    });
    await flush();
    h.read(async (path) =>
      path === '/versions' ? newVersions.promise : { instances: ['current'], last_instance_id: 'current' },
    );
    const current = h.machine.applyInstallQueueResponse(next);
    await flush();
    oldVersions.resolve({ versions: ['old'] });
    await flush();
    assert.equal(ready, false);
    assert.equal(h.store.instances.value.length, 0);
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
      try {
        return await reply;
      } finally {
        active -= 1;
      }
    };
    h.read((path) => track(path === '/versions' ? oldVersions.promise : oldInstances.promise));
    const old = h.machine.applyInstallQueueResponse(queue('first', 1, 0));
    await flush();
    h.read((path) =>
      track(
        Promise.resolve(
          path === '/versions' ? { versions: ['latest'] } : { instances: ['latest'], last_instance_id: 'latest' },
        ),
      ),
    );
    const middle = h.machine.applyInstallQueueResponse(queue('first', 2, 1));
    const latest = h.machine.applyInstallQueueResponse(queue('first', 3, 2));
    await flush();
    assert.equal(h.calls.length, 2, 'new cursors must join both active requests');
    assert.equal(h.store.instances.value.length, 0);

    if (failure === '/instances') oldInstances.reject(new Error('Obsolete instance read failed'));
    else if (failure === '/versions') oldVersions.reject(new Error('Obsolete version read failed'));
    else oldVersions.resolve({ versions: ['obsolete'] });
    await flush();
    assert.equal(h.calls.length, 2, 'one settled request must not release the other active request');
    assert.equal(h.store.instances.value.length, 0);
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

test('required hydration propagates a shared read failure and explicit retry bypasses background backoff', async () => {
  const h = downloadsHarness();
  const versionsReply = deferred<unknown>();
  h.read(async (path) =>
    path === '/install/queue'
      ? queue('first', 1, 0)
      : path === '/versions'
        ? versionsReply.promise
        : { instances: ['ready'], last_instance_id: null },
  );
  const initial = assert.rejects(h.machine.refreshInstallQueue({ requireInstalledState: true }), /Unavailable/);
  const background = h.machine.applyInstallQueueResponse(queue('first', 1, 0));
  await flush();
  versionsReply.reject(new Error('Unavailable'));
  await Promise.all([initial, background]);
  assert.equal(h.store.instances.value.length, 0);
  assert.equal(h.clock.timeouts.size, 1);
  h.read(async (path) =>
    path === '/install/queue'
      ? queue('first', 1, 0)
      : path === '/versions'
        ? { versions: ['ready'] }
        : { instances: ['ready'], last_instance_id: null },
  );
  await h.machine.refreshInstallQueue({ requireInstalledState: true });
  assert.deepEqual(h.store.instances.value, ['ready']);
  assert.equal(h.clock.timeouts.size, 0);
  assert.equal(h.calls.filter((path) => path === '/instances').length, 2);
});

test('a required attempt cannot reuse a projection read that began before the attempt at the same queue cursor', async () => {
  const h = downloadsHarness();
  const oldVersions = deferred<unknown>();
  h.read(async (path) =>
    path === '/versions' ? oldVersions.promise : { instances: ['old'], last_instance_id: 'old' },
  );
  const oldRead = h.machine.applyInstallQueueResponse(queue('first', 1, 0));
  await flush();
  h.read(async (path) =>
    path === '/install/queue'
      ? queue('first', 1, 0)
      : path === '/versions'
        ? { versions: ['fresh'] }
        : { instances: ['fresh'], last_instance_id: 'fresh' },
  );
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
  h.read(async (path) =>
    path === '/install/queue'
      ? queue('first', 1, 0)
      : path === '/versions'
        ? oldVersions.promise
        : { instances: ['old'], last_instance_id: 'old' },
  );
  const initial = assert.rejects(
    h.machine.refreshInstallQueue({ requireInstalledState: true }),
    /New projection unavailable/,
  );
  await flush();
  h.read(async () => {
    throw new Error('New projection unavailable');
  });
  const current = h.machine.applyInstallQueueResponse(queue('first', 2, 1));
  oldVersions.resolve({ versions: ['old'] });
  await Promise.all([initial, current]);
  assert.equal(h.store.instances.value.length, 0);
  assert.equal(h.calls.filter((path) => path === '/instances').length, 2);
  assert.equal(h.clock.timeouts.size, 1);
  h.machine.disconnectInstallQueue();
});

test('disconnecting a required projection discards its late read and does not restart it behind the caller', async () => {
  const h = downloadsHarness();
  const versionsReply = deferred<unknown>();
  h.read(async (path) =>
    path === '/install/queue'
      ? queue('first', 1, 0)
      : path === '/versions'
        ? versionsReply.promise
        : { instances: ['late'], last_instance_id: 'late' },
  );
  const stopped = assert.rejects(h.machine.refreshInstallQueue({ requireInstalledState: true }), /interrupted/);
  await flush();
  h.machine.disconnectInstallQueue();
  versionsReply.resolve({ versions: ['late'] });
  await stopped;
  assert.equal(h.store.instances.value.length, 0);
  assert.equal(h.calls.filter((path) => path === '/instances').length, 1);
});

test('a fresh required read after disconnect still drains the prior backend requests', async () => {
  const h = downloadsHarness();
  const oldVersions = deferred<unknown>();
  const oldInstances = deferred<unknown>();
  h.read(async (path) => (path === '/versions' ? oldVersions.promise : oldInstances.promise));
  const oldRead = h.machine.applyInstallQueueResponse(queue('first', 1, 0));
  await flush();
  h.machine.disconnectInstallQueue();
  h.read(async (path) =>
    path === '/install/queue'
      ? queue('first', 1, 0)
      : path === '/versions'
        ? { versions: ['fresh'] }
        : { instances: ['fresh'], last_instance_id: 'fresh' },
  );
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
  const downloads = downloadsHarness(launch);
  downloads.read(async (path) =>
    path === '/versions' ? { versions: [] } : { instances: [instance(false)], last_instance_id: 'instance-1' },
  );
  await downloads.machine.applyInstallQueueResponse(queue('first', 1, 0));

  // Removing a waiting install can advance the registry while this game runs.
  // Its instance read finishes first; the paired versions read is still pending.
  const oldVersions = deferred<unknown>();
  downloads.read(async (path) =>
    path === '/versions' ? oldVersions.promise : { instances: [instance(false)], last_instance_id: 'instance-1' },
  );
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

  downloads.read(async (path) =>
    path === '/versions' ? { versions: [] } : { instances: [instance(true)], last_instance_id: 'instance-1' },
  );
  oldVersions.resolve({ versions: [] });
  await oldProjection;
  assert.equal(launch.errors.length + downloads.errors.length, 0);
  assert.equal(launch.store.launchSessions.value['instance-1'], undefined);
  assert.equal(
    launch.store.instances.value[0].launch_action.label,
    'Launch',
    "the delayed registry response must not restore the ended session's Unavailable readiness",
  );
  assert.equal(launch.store.instances.value[0].launchable, true);
  assert.equal(downloads.calls.filter((path) => path === '/instances').length, 3);
  assert.equal(downloads.clock.timeouts.size, 0);
});

test('a complete session lifetime invalidates an older projection while terminal readiness is still pending', async () => {
  const launch = launchHarness(undefined, false);
  const downloads = downloadsHarness(launch);
  const oldVersions = deferred<unknown>();
  const oldInstances = deferred<unknown>();
  const terminalReadiness = deferred<unknown>();
  downloads.read(async (path) => (path === '/versions' ? oldVersions.promise : oldInstances.promise));
  const projection = downloads.machine.applyInstallQueueResponse(queue('first', 1, 0));
  await flush();

  launch.actions.confirmLaunch('instance-1', liveSession(status(), launch));
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

  downloads.read(async (path) =>
    path === '/versions' ? { versions: [] } : { instances: [instance(true)], last_instance_id: null },
  );
  oldVersions.resolve({ versions: [] });
  await projection;
  terminalReadiness.resolve(instance(true));
  await flush();
  assert.equal(launch.store.instances.value[0].launch_action.label, 'Launch');
  assert.equal(downloads.calls.filter((path) => path === '/instances').length, 2);
  assert.equal(downloads.errors.length + launch.errors.length, 0);
  assert.equal(downloads.clock.timeouts.size, 0);
});

for (const change of [
  'account selection',
  'config',
  'instance edit',
  'instance removal',
  'versions',
  'last instance',
  'preparation lifetime',
] as const) {
  test(`a downloads projection rebases after a newer ${change} without advancing the queue cursor`, async () => {
    const launch = launchHarness(undefined, false);
    launch.actions.setConfig(configSnapshot());
    const downloads = downloadsHarness(launch);
    const oldVersions = deferred<unknown>();
    downloads.read(async (path) =>
      path === '/versions' ? oldVersions.promise : { instances: [instance(false)], last_instance_id: null },
    );
    const projection = downloads.machine.applyInstallQueueResponse(queue('first', 1, 0));
    await flush();
    if (change === 'account selection')
      launch.actions.setConfig({ ...configSnapshot(), account_selection_revision: 2 });
    if (change === 'config') launch.actions.setConfig({ ...configSnapshot(), revision: 2, max_memory_mb: 8192 });
    if (change === 'instance edit') launch.actions.updateInstanceInList({ ...instance(true), name: 'Renamed' });
    if (change === 'instance removal') launch.actions.removeInstance('instance-1');
    if (change === 'versions') launch.store.versions.value = [];
    if (change === 'last instance') launch.store.lastInstanceId.value = 'instance-1';
    if (change === 'preparation lifetime') {
      launch.actions.startLaunch('instance-1');
      launch.actions.endLaunchPrep();
    }
    const expected =
      change === 'instance removal'
        ? []
        : [{ ...instance(true), name: change === 'instance edit' ? 'Renamed' : 'Example' }];
    const lastInstance = launch.store.lastInstanceId.value;
    downloads.read(async (path) =>
      path === '/versions' ? { versions: [] } : { instances: expected, last_instance_id: lastInstance },
    );
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
  const downloads = downloadsHarness(launch);
  launch.actions.startLaunch('instance-2');
  const versionsReply = deferred<unknown>();
  downloads.read(async (path) =>
    path === '/versions' ? versionsReply.promise : { instances: [instance(false)], last_instance_id: null },
  );
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
  const downloads = downloadsHarness(launch);
  const oldVersions = deferred<unknown>();
  const freshVersions = deferred<unknown>();
  downloads.read(async (path) =>
    path === '/install/queue'
      ? queue('first', 1, 0)
      : path === '/versions'
        ? oldVersions.promise
        : { instances: [instance(false)], last_instance_id: null },
  );
  let ready = false;
  const initial = downloads.machine.refreshInstallQueue({ requireInstalledState: true }).then(() => {
    ready = true;
  });
  await flush();
  launch.actions.setConfig(configSnapshot());
  downloads.read(async (path) =>
    path === '/versions' ? freshVersions.promise : { instances: [instance(true)], last_instance_id: null },
  );
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
  const downloads = downloadsHarness(launch);
  const replies = [deferred<unknown>(), deferred<unknown>()];
  let attempt = 0;
  downloads.read(async (path) =>
    path === '/install/queue'
      ? queue('first', 1, 0)
      : path === '/versions'
        ? replies[attempt++].promise
        : { instances: [instance(false)], last_instance_id: null },
  );
  const failed = assert.rejects(
    downloads.machine.refreshInstallQueue({ requireInstalledState: true }),
    /state changed while refreshing/,
  );
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
  downloads.read(async (path) =>
    path === '/versions' ? { versions: [] } : { instances: [instance(true)], last_instance_id: null },
  );
  downloads.clock.runTimeout();
  await flush();
  assert.equal(launch.store.instances.value[0].launch_action.label, 'Launch');
  assert.equal(downloads.calls.filter((path) => path === '/instances').length, 3);
  assert.equal(downloads.clock.timeouts.size, 0);
});

test('disconnect during a rebase prevents late publication and cannot schedule a retry', async () => {
  const launch = launchHarness(undefined, false);
  const downloads = downloadsHarness(launch);
  const first = deferred<unknown>();
  const second = deferred<unknown>();
  downloads.read(async (path) =>
    path === '/versions' ? first.promise : { instances: [instance(false)], last_instance_id: null },
  );
  const projection = downloads.machine.applyInstallQueueResponse(queue('first', 1, 0));
  await flush();
  launch.actions.setConfig(configSnapshot());
  downloads.read(async (path) =>
    path === '/versions' ? second.promise : { instances: [instance(true)], last_instance_id: null },
  );
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

test('queue revisions beyond exact JavaScript ordering are refused before publication', async () => {
  const harness = downloadsHarness();
  await harness.machine.applyInstallQueueResponse(queue('first', 1, 0));
  harness.calls.length = 0;
  for (const response of [
    queue('first', Number.MAX_SAFE_INTEGER + 1, 0),
    queue('first', 2, Number.MAX_SAFE_INTEGER + 1),
  ]) {
    await assert.rejects(harness.machine.applyInstallQueueResponse(response), /outside the range/);
  }
  assert.equal(harness.machine.downloadQueue.value.view_model.status_label, 'first');
  assert.equal(harness.calls.length, 0);
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
  harness.read(async (path) =>
    path === '/install/queue'
      ? initial.promise
      : path === '/versions'
        ? { versions: [] }
        : { instances: [], last_instance_id: null },
  );
  const refresh = harness.machine.refreshInstallQueue();
  await harness.machine.applyInstallQueueResponse(queue('new', 1, 0));
  initial.resolve(queue('old', 80, 9));
  await refresh;
  assert.equal(harness.machine.downloadQueue.value.view_model.status_label, 'new');
});

test('delayed registry reads from an old process cannot overwrite the restarted process views', async () => {
  const harness = downloadsHarness();
  const oldVersions = deferred<unknown>();
  harness.read(async (path) =>
    path === '/versions' ? oldVersions.promise : { instances: ['old'], last_instance_id: 'old' },
  );
  const oldRefresh = harness.machine.applyInstallQueueResponse(queue('old', 80, 9));
  await flush();
  harness.read(async (path) =>
    path === '/versions' ? { versions: ['new'] } : { instances: ['new'], last_instance_id: 'new' },
  );
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

test('failures are listed newest first and leave with the queue snapshot that drops them', async () => {
  const harness = downloadsHarness();
  const view: InstallFailureViewModel = {
    state_id: 'failed',
    title: 'Install failed',
    tone: 'err',
    summary: 'Download interrupted',
    details: [],
    retry_action: { action: 'retry', label: 'Retry install', enabled: true },
    dismiss_action: { action: 'dismiss', label: 'Dismiss', enabled: true },
  };
  const failed = (operation: string, label: string): InstallQueueFailureViewModel => ({
    failed_at_ms: 100,
    queue_id: operation,
    install_id: operation,
    operation_id: operation,
    label,
    install_item: { version_id: '1.21.6' },
    failure_view_model: view,
  });
  const names = () => harness.machine.downloadFailures.value.map((failure) => failure.displayName);
  await harness.machine.applyInstallQueueResponse({
    ...queue('first', 1, 0),
    failures: [failed('operation-2', 'Minecraft 1.21.6'), failed('operation-1', 'Minecraft 1.20.1')],
  });
  assert.deepEqual(names(), ['Minecraft 1.21.6', 'Minecraft 1.20.1']);
  await harness.machine.applyInstallQueueResponse({
    ...queue('first', 2, 0),
    failures: [failed('operation-1', 'Minecraft 1.20.1')],
  });
  assert.deepEqual(names(), ['Minecraft 1.20.1']);
  await harness.machine.applyInstallQueueResponse({
    ...queue('first', 1, 0),
    failures: [failed('operation-2', 'Minecraft 1.21.6')],
  });
  assert.deepEqual(names(), ['Minecraft 1.20.1']);
  await harness.machine.applyInstallQueueResponse(queue('first', 3, 0));
  assert.deepEqual(names(), []);
});

test('new event connections deliver their complete snapshot even when revisions restart', async () => {
  const clock = timers();
  const sources: FakeEventSource[] = [];
  const seen: number[] = [];
  class FakeEventSource {
    readonly listeners = new Map<string, (event: { data: string }) => void>();
    onopen?: () => void;
    onerror?: () => void;
    constructor() {
      sources.push(this);
    }
    addEventListener(name: string, callback: (event: { data: string }) => void): void {
      this.listeners.set(name, callback);
    }
    close(): void {}
    emit(revision: number): void {
      this.listeners.get('message')?.({ data: JSON.stringify({ revision, value: revision }) });
    }
  }
  const events = source<typeof import('../src/platform/events')>(
    'platform/events.ts',
    {
      'platform/transport': { apiEventSourceUrl: async () => '/fresh-ticket' },
    },
    { ...clock, EventSource: FakeEventSource },
  );
  const close = events.subscribeApiEvents<number>('/install/queue/events', {
    onValue: (value) => seen.push(value),
  });
  await flush();
  sources[0].emit(80);
  sources[0].emit(79);
  sources[0].onerror?.();
  clock.runTimeout();
  await flush();
  sources[0].emit(100);
  sources[1].emit(1);
  sources[1].emit(1);
  assert.deepEqual(seen, [80, 1]);
  close();
});

test('Downloads renders loader labels and Minecraft versions while retaining queue identity', async () => {
  const h = downloadsHarness();
  const snapshot = queue('first', 1, 0);
  const item = {
    version_id: 'loader-v2-opaque-installed-identity',
    loader: {
      component_id: 'net.fabricmc.fabric-loader',
      build_id: 'opaque-build-identity',
      minecraft_version: '1.20.1',
      loader_version: '0.19.5',
    },
  } as const;
  const label = 'Fabric 0.19.5 for Minecraft 1.20.1';
  snapshot.active = {
    queue_id: 'active-queue',
    kind: 'loader',
    title: label,
    label,
    summary: 'Downloading files',
    install_item: item,
    progress: { phase_id: 'download', label: 'Downloading files', progress_pct: 25, terminal: false, failed: false },
  };
  snapshot.items = [
    {
      queue_id: 'next-queue',
      state_id: 'queued',
      kind: 'loader',
      title: label,
      label,
      summary: 'Waiting to install',
      detail: '',
      position: 1,
      total: 1,
      install_item: item,
      remove_action: { action: 'remove_from_queue', label: 'Remove from queue', enabled: true },
    },
  ];
  snapshot.view_model.queued_count = 1;
  await h.machine.applyInstallQueueResponse(snapshot);
  assert.equal(h.machine.activeDownload.value?.item.version_id, item.version_id);
  const removed: string[] = [];
  const { DownloadsView } = source<typeof import('../src/install/DownloadsView')>('install/DownloadsView.tsx', {
    'preact/jsx-runtime': requireDependency('preact/jsx-runtime'),
    'ui/Atoms': { Button: 'Button', Chip: 'Chip', Empty: 'Empty', Meter: 'Meter', Ring: 'Ring' },
    'ui/Face': { Face: 'Face' },
    'ui/Icons': { Icon: 'Icon' },
    'ui/InstanceVisual': { InstanceTile: 'InstanceTile' },
    'instances/state': { instances: h.store.instances },
    'app/route': { navigate() {}, openCreate() {} },
    'ui/use-now': { useNowTicker: () => 0 },
    'discover/shared': { KIND_ICON: {} },
    'create/defaults': { loaderKeyFromComponentId: () => 'fabric' },
    'create/loader-logos': { LoaderLogo: 'LoaderLogo' },
    'install/queue': { ...h.machine, removeQueuedInstall: (id: string) => removed.push(id) },
  });
  type Node = { type: unknown; props: Record<string, unknown> };
  function nodes(value: unknown): Node[] {
    if (Array.isArray(value)) return value.flatMap(nodes);
    if (!value || typeof value !== 'object' || !('props' in value)) return [];
    const node = value as Node;
    if (typeof node.type === 'function' && ['Row', 'FailureRow', 'Lead', 'Target'].includes(node.type.name))
      return nodes(node.type(node.props));
    return [node, ...nodes(node.props.children)];
  }
  const rendered = nodes(DownloadsView());
  assert.equal(rendered.find((node) => node.type === 'strong')?.props.children, label);
  assert.equal(rendered.filter((node) => node.type === 'strong' && node.props.children === label).length, 2);
  assert.equal(rendered.find((node) => node.type === 'Chip')?.props.children, '1.20.1');
  assert.equal(
    rendered.some((node) => node.props.children === item.version_id),
    false,
  );
  const remove = rendered.find((node) => node.type === 'button' && node.props['aria-label'] === 'Remove from queue');
  assert.ok(remove);
  (remove.props.onClick as () => void)();
  assert.deepEqual(removed, ['next-queue']);
});

test('install ring estimates advance only on accepted snapshots and reset for each owner', async () => {
  const h = downloadsHarness();
  const snapshot = (epoch: string, revision: number, phase: string, pct: number, install = 'first') => ({
    ...queue(epoch, revision, 0),
    active: {
      queue_id: 'same-queue',
      install_id: install,
      operation_id: install,
      kind: 'vanilla' as const,
      title: 'Minecraft',
      label: 'Minecraft',
      summary: '',
      install_item: { version_id: '1.21.4' },
      progress: { phase_id: phase, label: phase, progress_pct: pct, terminal: false, failed: false },
    },
  });
  await h.machine.applyInstallQueueResponse(snapshot('epoch-1', 1, 'assets', 50));
  const first = h.machine.activeDownload.value!;
  assert.equal(first.pct, 50);
  assert.ok(first.ringPct > 35);
  assert.equal(h.machine.activeDownload.value!.ringPct, first.ringPct);
  await h.machine.applyInstallQueueResponse(snapshot('epoch-1', 2, 'client', 10));
  assert.equal(h.machine.activeDownload.value!.ringPct, first.ringPct);
  assert.equal(h.machine.activeDownload.value!.pct, 10);
  await h.machine.applyInstallQueueResponse(snapshot('epoch-1', 1, 'processors', 100));
  assert.equal(h.machine.activeDownload.value!.ringPct, first.ringPct);
  await h.machine.applyInstallQueueResponse(snapshot('epoch-1', 3, 'client', 0, 'retry'));
  assert.equal(h.machine.activeDownload.value!.ringPct, 3);
  await h.machine.applyInstallQueueResponse(snapshot('epoch-2', 1, 'version_json', 0, 'retry'));
  assert.equal(h.machine.activeDownload.value!.ringPct, 0);
});

test('fallback polling stops only when a stream snapshot converges on this incarnation', async () => {
  const h = launchHarness();
  await flush();
  const intervals = (): number => h.clock.intervals.size;
  assert.equal(intervals(), 1, 'polling covers the stream until it converges');
  h.emit(status(2), 'status');
  await flush();
  for (const stale of [
    { ...status(9), launched_at: '2026-09-08T07:00:00Z' },
    { ...status(9), session_id: 'session-2' },
    status(1),
  ]) {
    h.emit(stale, 'status');
    await flush();
  }
  assert.equal(intervals(), 0, 'the convergent snapshot at revision 2 ended polling');
  h.interruptStream();
  await flush();
  assert.equal(intervals(), 1, 'a disconnected stream resumes polling');
  h.emit({ ...status(9), launched_at: '2026-09-08T07:00:00Z' }, 'status');
  h.emit(status(1), 'status');
  await flush();
  assert.equal(intervals(), 1, 'a foreign incarnation or older replay cannot stop polling');
  h.emit(status(3), 'status');
  await flush();
  assert.equal(intervals(), 0);
});
