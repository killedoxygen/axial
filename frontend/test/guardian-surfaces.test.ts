import assert from 'node:assert/strict';
import test from 'node:test';

import {
  clearLaunchNotice,
  confirmLaunch,
  convergeLaunchStatus,
  launchNotices,
  launchSessions,
  setLaunchNotice,
  updateLaunchSessionState,
} from '../src/launch/state';
import { createResultToastMessage, createToastKind } from '../src/create/presenters';
import { installQueueNoticePresentation } from '../src/install/presenters';
import { createLaunchNoticeTracker } from '../src/launch/notices';
import {
  launchActionPresentation,
  launchSessionActivityLabel,
  launchSessionCanStop,
  launchSessionHasLiveProcess,
  launchSessionIsPlaying,
} from '../src/launch/presenters';
import type { InstallQueueNoticeViewModel } from '../src/generated/InstallQueueNoticeViewModel';
import type { SessionNotice } from '../src/generated/SessionNotice';
import type { SessionOutcome } from '../src/generated/SessionOutcome';
import type { SessionSnapshot } from '../src/generated/SessionSnapshot';
import type { SessionViewModel } from '../src/generated/SessionViewModel';
import type { LaunchSession } from '../src/launch/state';

function launchViewModel(stateId: string, label: string, overrides: Partial<SessionViewModel> = {}): SessionViewModel {
  return {
    state_id: stateId,
    label,
    progress_pct: 88,
    terminal: false,
    playing: false,
    process_live: false,
    can_stop: false,
    ...overrides,
  };
}

function launchWireStatus(
  sessionId: string,
  revision: number,
  viewModel: SessionViewModel,
  outcome: SessionOutcome | null = null,
  notice: SessionNotice | null = null,
): SessionSnapshot {
  return {
    session_id: sessionId,
    instance_id: 'instance-a',
    revision,
    phase: viewModel.terminal ? 'exited' : 'running',
    launched_at: '2026-07-17T00:00:00.000Z',
    started_at_ms: null,
    pid: null,
    process_alive: viewModel.process_live,
    stop_allowed: viewModel.can_stop,
    exit_code: null,
    tree_settled: viewModel.terminal,
    output_drained: viewModel.terminal,
    boot_observed: false,
    outcome,
    notice,
    view_model: viewModel,
  };
}

test('launch notice tracker keeps dismissal across duplicate transports', () => {
  launchNotices.value = {};
  const tracker = createLaunchNoticeTracker();
  const initial: SessionNotice = { message: 'Session cleanup is unfinished.', tone: 'warn' };

  const presented = tracker.consume(initial);
  assert.ok(presented);
  setLaunchNotice('instance-a', presented);
  clearLaunchNotice('instance-a');

  assert.equal(tracker.consume({ ...initial }), null);
  assert.equal(tracker.consume({ ...initial, message: `  ${initial.message}  ` }), null);
  assert.equal(launchNotices.value['instance-a'], undefined);
  launchNotices.value = {};
});

test('launch notice tracker accepts distinct live and terminal notices while suppressing replay', () => {
  const tracker = createLaunchNoticeTracker();
  const liveA: SessionNotice = { message: 'Session cleanup is unfinished.', tone: 'warn' };
  const liveB: SessionNotice = { message: 'Launch report unavailable.', tone: 'warn' };
  const terminal: SessionNotice = { message: 'Minecraft stopped during startup.', tone: 'err' };

  assert.deepEqual(tracker.consume(liveA), liveA);
  assert.deepEqual(tracker.consume(liveB), liveB);
  assert.equal(tracker.consume(liveA), null);
  assert.equal(tracker.consume(liveB), null);
  assert.deepEqual(tracker.consume(terminal), terminal);
  assert.equal(tracker.consume({ ...terminal }), null);
  assert.equal(tracker.consume(null), null);
  assert.equal(tracker.consume({ message: '  ', tone: 'info' }), null);
});

test('launch notice tracker resets for a fresh session', () => {
  const notice: SessionNotice = { message: 'Minecraft closed normally.', tone: 'info' };
  const firstSession = createLaunchNoticeTracker();
  assert.ok(firstSession.consume(notice));
  assert.equal(firstSession.consume(notice), null);
  assert.deepEqual(createLaunchNoticeTracker().consume(notice), notice);
});

test('launch action presentation follows backend launch, install, blocked, queued, and progress states', () => {
  const base = { installQueued: false, installQueuedView: undefined, installProgress: null, preparing: null };
  assert.deepEqual(
    launchActionPresentation({
      ...base,
      launchAction: {
        state_id: 'launch_ready',
        label: 'Launch',
        tone: 'ok',
        launchable: true,
        primary_action: 'launch',
      },
    }),
    {
      progress: null,
      usesInstallAction: false,
      blocked: false,
      label: 'Launch',
      icon: 'play',
      pct: 0,
      disabled: false,
    },
  );

  const install = launchActionPresentation({
    ...base,
    launchAction: {
      state_id: 'install_required',
      label: 'Install required files',
      tone: 'warn',
      launchable: false,
      primary_action: 'install',
    },
  });
  assert.equal(install.label, 'Install required files');
  assert.equal(install.icon, 'download');
  assert.equal(install.usesInstallAction, true);
  assert.equal(install.disabled, false);

  const blocked = launchActionPresentation({
    ...base,
    launchAction: {
      state_id: 'repair_required',
      label: 'Repair required',
      tone: 'err',
      launchable: false,
      primary_action: 'blocked',
      disabled_reason: 'Backend-authored repair guidance.',
    },
  });
  assert.equal(blocked.label, 'Repair required');
  assert.equal(blocked.icon, 'alert');
  assert.equal(blocked.blocked, true);
  assert.equal(blocked.disabled, true);

  const queued = launchActionPresentation({
    ...base,
    launchAction: {
      state_id: 'install_required',
      label: 'Install',
      tone: 'warn',
      launchable: false,
      primary_action: 'install',
    },
    installQueued: true,
    installQueuedView: { title: 'Retry queued', summary: 'Backend queue summary.' },
  });
  assert.equal(queued.label, 'Retry queued');
  assert.equal(queued.icon, 'clock');
  assert.equal(queued.disabled, true);

  const progress = launchActionPresentation({
    ...base,
    launchAction: {
      state_id: 'launch_ready',
      label: 'Launch',
      tone: 'ok',
      launchable: true,
      primary_action: 'launch',
    },
    preparing: {
      status: 'preparing',
      instanceId: 'instance-a',
      pct: 37,
      label: 'Backend launch preparation',
      determinate: true,
    },
  });
  assert.equal(progress.label, 'Backend launch preparation');
  assert.equal(progress.pct, 37);
  assert.equal(progress.disabled, true);
});

test('launch session patches and notice dismissal remain independently scoped', () => {
  launchSessions.value = {};
  launchNotices.value = {};
  const session: LaunchSession = {
    sessionId: 'session-a',
    launchedAt: '2026-07-17T00:00:00.000Z',
    viewModel: launchViewModel('queued', 'Preparing launch', { progress_pct: 8 }),
    statusRevision: 0,
  };

  confirmLaunch('instance-a', session);
  updateLaunchSessionState('instance-a', { stopping: true });
  assert.equal(launchSessions.value['instance-a']?.viewModel?.label, 'Preparing launch');

  const first: SessionNotice = { message: 'First backend notice', tone: 'warn' };
  const second: SessionNotice = { message: 'Second backend notice', tone: 'info' };
  setLaunchNotice('instance-a', first);
  setLaunchNotice('instance-b', second);
  clearLaunchNotice('instance-a');
  assert.equal(launchNotices.value['instance-a'], undefined);
  assert.equal(launchNotices.value['instance-b'], second);

  launchSessions.value = {};
  launchNotices.value = {};
});

test('launch session presentation distinguishes active recovery from a live process', () => {
  const session = (state: string, viewModel: SessionViewModel): LaunchSession => ({
    sessionId: `session-${state}`,
    launchedAt: '2026-07-17T00:00:00.000Z',
    viewModel,
    statusRevision: 0,
  });

  const recovering = session('recovering', launchViewModel('recovering', 'Guardian is repairing startup'));
  assert.equal(launchSessionIsPlaying(recovering), false);
  assert.equal(launchSessionCanStop(recovering), false);
  assert.equal(launchSessionHasLiveProcess(recovering), false);
  assert.equal(launchSessionActivityLabel(recovering), 'Guardian is repairing startup');

  for (const state of ['queued', 'planning', 'preparing', 'settling']) {
    const label = state === 'settling' ? 'Finalizing session' : 'Preparing launch';
    const current = session(state, launchViewModel(state, label));
    assert.equal(launchSessionIsPlaying(current), false, state);
    assert.equal(launchSessionCanStop(current), false, state);
    assert.equal(launchSessionHasLiveProcess(current), false, state);
  }
  for (const state of ['starting', 'monitoring']) {
    const current = session(
      state,
      launchViewModel(state, 'Starting Minecraft', { process_live: true, can_stop: true }),
    );
    assert.equal(launchSessionIsPlaying(current), false, state);
    assert.equal(launchSessionCanStop(current), true, state);
    assert.equal(launchSessionHasLiveProcess(current), true, state);
  }
  for (const state of ['running', 'degraded']) {
    const label = state === 'degraded' ? 'Running with warnings' : 'Playing';
    const current = session(
      state,
      launchViewModel(state, label, { playing: true, process_live: true, can_stop: true }),
    );
    assert.equal(launchSessionIsPlaying(current), true, state);
    assert.equal(launchSessionCanStop(current), true, state);
    assert.equal(launchSessionHasLiveProcess(current), true, state);
    assert.equal(launchSessionActivityLabel(current), label);
  }
});

test('launch session convergence rejects stale and replacement-session updates atomically', () => {
  const session: LaunchSession = {
    sessionId: 'ordered-session',
    launchedAt: '2026-07-17T00:00:00.000Z',
    viewModel: launchViewModel('monitoring', 'Monitoring startup', { process_live: true, can_stop: true }),
    statusRevision: 7,
  };
  launchSessions.value = { 'instance-a': session };

  assert.equal(
    convergeLaunchStatus(
      'instance-a',
      'ordered-session',
      launchWireStatus(
        'ordered-session',
        7,
        launchViewModel('exited', 'Exited', { progress_pct: 100, terminal: true }),
        { reason: 'clean_exit', kind: 'clean', failure_class: null, summary: 'Minecraft closed normally.' },
      ),
    ),
    null,
  );
  assert.equal(launchSessions.value['instance-a']?.statusRevision, 7);
  const recovery = convergeLaunchStatus(
    'instance-a',
    'ordered-session',
    launchWireStatus('ordered-session', 8, launchViewModel('recovering', 'Recovering startup')),
  );
  assert.equal(recovery?.revision, 8);
  assert.equal(launchSessions.value['instance-a']?.viewModel.state_id, 'recovering');
  assert.equal(launchSessions.value['instance-a']?.statusRevision, 8);
  assert.equal(
    convergeLaunchStatus(
      'instance-a',
      'ordered-session',
      launchWireStatus(
        'other-session',
        9,
        launchViewModel('running', 'Running', { playing: true, process_live: true, can_stop: true }),
      ),
    ),
    null,
  );
  const running = convergeLaunchStatus(
    'instance-a',
    'ordered-session',
    launchWireStatus(
      'ordered-session',
      9,
      launchViewModel('running', 'Running', { playing: true, process_live: true, can_stop: true }),
      null,
      { message: 'Guardian completed recovery.', tone: 'ok' },
    ),
  );
  assert.equal(running?.notice?.message, 'Guardian completed recovery.');
  assert.equal(launchSessions.value['instance-a']?.viewModel.playing, true);
  launchSessions.value = {};
});

// Transport failure/polling and operation-scoped failure dismissal are exercised
// against the API owners in session-download-recovery.test.ts.

test('install queue notices render backend copy and backend action tone for every current notice state', () => {
  const fixtures: Array<[string, InstallQueueNoticeViewModel['tone'], 'success' | 'error' | 'info']> = [
    ['queued', 'info', 'success'],
    ['retry_queued', 'info', 'success'],
    ['already_active', 'info', 'info'],
    ['already_queued', 'info', 'info'],
    ['retry_moved_next', 'info', 'info'],
    ['queue_failed', 'err', 'error'],
    ['queue_warning', 'warn', 'info'],
  ];
  for (const [stateId, tone, kind] of fixtures) {
    const notice: InstallQueueNoticeViewModel = {
      state_id: stateId,
      tone,
      message: ` ${stateId} message `,
      detail: ` ${stateId} detail `,
    };
    assert.deepEqual(installQueueNoticePresentation(notice), {
      message: `${stateId} message: ${stateId} detail`,
      kind,
    });
  }
  assert.equal(installQueueNoticePresentation({ state_id: 'idle', tone: 'info', message: '   ' }), null);
});

test('create surfaces preserve backend display copy', () => {
  assert.equal(
    createResultToastMessage({
      state_id: 'created',
      tone: 'ok',
      title: 'Instance created',
      summary: 'Instance created.',
      detail: 'Install queued.',
    }),
    'Instance created. Install queued.',
  );
  assert.equal(createToastKind('err'), 'error');
  assert.equal(createToastKind('warn'), 'info');
  assert.equal(createToastKind('ok'), 'success');
});
