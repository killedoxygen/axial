import assert from 'node:assert/strict';
import test from 'node:test';

import {
  clearLaunchNotice,
  confirmLaunch,
  convergeLaunchStatus,
  endSessionIfCurrent,
  setLaunchNotice,
  updateLaunchSessionState,
} from '../src/actions';
import {
  createNoticePresentation,
  createResultToastMessage,
  createToastKind,
  type CreateNotice,
  type CreateResultPresentationSource,
} from '../src/create-presenters';
import {
  clearDownloadFailure,
  clearDownloadFailureForItem,
  downloadFailure,
  recordDownloadFailure,
} from '../src/machines/downloads';
import {
  installFailureViewModel,
  installQueueNoticePresentation,
  unresolvedFailureViewModel,
} from '../src/machines/download-view-models';
import { backendLaunchNotice, createBackendLaunchNoticeTracker } from '../src/launch-notice-tracker';
import { establishNativeLaunchTransport, type LaunchLiveHandle } from '../src/launch-live-transport';
import {
  launchActionPresentation,
  launchNoticePresentation,
  launchSessionActivityLabel,
  launchSessionCanStop,
  launchSessionHasLiveProcess,
  launchSessionIsPlaying,
} from '../src/launch-presenters';
import { launchProofGuardianEvidence } from '../src/launch-proof-presenters';
import { launchSessionOutcome, launchStatusUpdate, launchStatusViewModel } from '../src/launch-response-adapters';
import { performanceHealthNotice } from '../src/performance-presenters';
import { GUARDIAN_OPTIONS, guardianModeFrom } from '../src/guardian-settings';
import { launchNotices, launchSessions } from '../src/store';
import { startupWarningMessages } from '../src/startup-warnings';
import type { InstallFailureViewModel, InstallItem, InstallQueueNoticeViewModel } from '../src/types-install';
import type { LaunchNotice, LaunchSession, LaunchStatusViewModel } from '../src/types-launch';

const sensitiveFragments = [
  '/home/alice/.axial',
  'C:\\Users\\Alice\\AppData',
  'raw-secret-token',
  '--accessToken',
  'java.exe',
  'at installWorker',
];

type LaunchProofFixture = Parameters<typeof launchProofGuardianEvidence>[0] & Record<string, unknown>;
type PerformanceHealthFixture = NonNullable<Parameters<typeof performanceHealthNotice>[0]> & Record<string, unknown>;

function launchViewModel(
  stateId: string,
  label: string,
  overrides: Partial<LaunchStatusViewModel> = {},
): LaunchStatusViewModel {
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
  viewModel: unknown,
  outcome: unknown = null,
  notice: unknown = null,
): Record<string, unknown> {
  return {
    session_id: sessionId,
    revision,
    view_model: viewModel,
    notice,
    outcome,
  };
}

function assertExcludesSensitive(value: unknown): void {
  const serialized = JSON.stringify(value);
  for (const fragment of sensitiveFragments) {
    assert.equal(serialized.includes(fragment), false, `surface leaked ${fragment}`);
  }
}

function failureFixture(stateId: string, retryEnabled: boolean): InstallFailureViewModel & { raw_error: string } {
  return {
    state_id: stateId,
    title: 'Install failed',
    tone: 'err',
    summary: `Backend summary for ${stateId}`,
    detail: `Backend detail for ${stateId}`,
    details: [`Backend detail for ${stateId}`, `Backend guidance for ${stateId}`],
    retry_action: {
      action: 'retry',
      label: retryEnabled ? 'Retry install' : 'Retry paused',
      enabled: retryEnabled,
      disabled_reason: retryEnabled ? null : 'Wait for the backend retry cooldown.',
    },
    dismiss_action: { action: 'dismiss', label: 'Dismiss', enabled: true, disabled_reason: null },
    raw_error: sensitiveFragments.join(' '),
  };
}

test('startup warnings retain backend copy, discard invalid entries, and deduplicate', () => {
  assert.deepEqual(
    startupWarningMessages([
      '  Guardian kept Axial running.  ',
      null,
      'Guardian kept Axial running.',
      '',
      42,
      'Instance registry started empty.',
    ]),
    ['Guardian kept Axial running.', 'Instance registry started empty.'],
  );
  assert.deepEqual(startupWarningMessages({ warnings: [] }), []);
});

test('launch notice adapters preserve backend copy for every rendered tone and ignore unknown raw fields', () => {
  const icons = {
    info: 'info',
    success: 'check-circle',
    warned: 'alert',
    intervened: 'shield-check',
    error: 'alert',
  } as const;

  for (const [tone, icon] of Object.entries(icons)) {
    const payload = {
      message: `Backend ${tone} message`,
      detail: `Backend ${tone} detail`,
      details: [`Backend ${tone} detail`, `Backend ${tone} guidance`, '', 4],
      tone,
      raw_error: sensitiveFragments.join(' '),
    };
    const notice = backendLaunchNotice(payload);
    assert.ok(notice);
    assert.deepEqual(notice, {
      message: payload.message,
      detail: payload.detail,
      details: [`Backend ${tone} detail`, `Backend ${tone} guidance`],
      tone,
    });
    assert.deepEqual(launchNoticePresentation(notice), {
      icon,
      primaryDetail: payload.detail,
      listDetails: [`Backend ${tone} guidance`],
    });
    assertExcludesSensitive(notice);
  }

  assert.equal(backendLaunchNotice({ message: 'No tone', tone: 'warning' }), null);
  assert.equal(backendLaunchNotice({ message: '', tone: 'error' }), null);
});

test('launch notice tracker seeds from the initial response and keeps dismissal across duplicate transports', () => {
  launchNotices.value = {};
  const tracker = createBackendLaunchNoticeTracker();
  const initial = {
    message: 'Guardian adjusted this launch.',
    detail: 'Managed Java was selected.',
    details: ['Managed Java was selected.'],
    tone: 'intervened',
  } as const;

  const presented = tracker.consume(initial);
  assert.ok(presented);
  setLaunchNotice('instance-a', presented);
  clearLaunchNotice('instance-a');

  assert.equal(tracker.consume({ ...initial }), null);
  assert.equal(
    tracker.consume({
      ...initial,
      message: `  ${initial.message}  `,
      detail: ` ${initial.detail} `,
      details: [` ${initial.details[0]} `],
    }),
    null,
  );
  assert.equal(launchNotices.value['instance-a'], undefined);
  launchNotices.value = {};
});

test('launch notice tracker surfaces late backend Healing copy and never infers from raw summaries', () => {
  const tracker = createBackendLaunchNoticeTracker();
  assert.equal(tracker.consume(null), null);
  assert.equal(
    tracker.consume({
      guardian: {
        decision: 'intervened',
        message: 'Raw Guardian summary must not become frontend copy.',
      },
      healing: {
        fallback_applied: 'Raw Healing summary must not become frontend copy.',
      },
      outcome: { kind: 'failed', summary: 'Raw outcome must not become frontend copy.' },
      raw_error: sensitiveFragments.join(' '),
    }),
    null,
  );

  const healingNotice = tracker.consume({
    message: 'Healing retried startup with safer settings.',
    detail: 'The backend selected a compatible preset.',
    tone: 'success',
    guardian: { message: 'Ignored raw Guardian copy.' },
    raw_error: sensitiveFragments.join(' '),
  });
  assert.deepEqual(healingNotice, {
    message: 'Healing retried startup with safer settings.',
    detail: 'The backend selected a compatible preset.',
    details: [],
    tone: 'success',
  });
  assertExcludesSensitive(healingNotice);
});

test('launch notice tracker accepts distinct live and terminal notices while suppressing stale and null replay', () => {
  const tracker = createBackendLaunchNoticeTracker();
  const liveA = { message: 'Guardian warned about launch settings.', tone: 'warned' } as const;
  const liveB = { message: 'Guardian applied a safer launch plan.', tone: 'intervened' } as const;
  const terminal = { message: 'Minecraft stopped during startup.', tone: 'error' } as const;

  assert.deepEqual(tracker.consume(liveA), { ...liveA, detail: '', details: [] });
  assert.deepEqual(tracker.consume(liveB), { ...liveB, detail: '', details: [] });
  assert.equal(tracker.consume(liveA), null);
  assert.equal(tracker.consume(liveB), null);
  assert.deepEqual(tracker.consume(terminal), { ...terminal, detail: '', details: [] });
  assert.equal(tracker.consume({ ...terminal }), null);
  assert.equal(tracker.consume(null), null);
});

test('launch notice tracker resets for a fresh session', () => {
  const notice = { message: 'Guardian selected managed Java.', tone: 'intervened' } as const;
  const firstSession = createBackendLaunchNoticeTracker();
  assert.ok(firstSession.consume(notice));
  assert.equal(firstSession.consume(notice), null);

  const nextSession = createBackendLaunchNoticeTracker();
  assert.deepEqual(nextSession.consume(notice), { ...notice, detail: '', details: [] });
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

  const first: LaunchNotice = { message: 'First backend notice', tone: 'warned' };
  const second: LaunchNotice = { message: 'Second backend notice', tone: 'intervened' };
  setLaunchNotice('instance-a', first);
  setLaunchNotice('instance-b', second);
  clearLaunchNotice('instance-a');
  assert.equal(launchNotices.value['instance-a'], undefined);
  assert.equal(launchNotices.value['instance-b'], second);

  launchSessions.value = {};
  launchNotices.value = {};
});

test('launch status and outcome adapters accept only bounded typed display fields', () => {
  assert.deepEqual(
    launchStatusViewModel({
      state_id: 'preparing_launch',
      label: 'Backend preparation copy',
      progress_pct: 140,
      terminal: false,
      playing: false,
      process_live: true,
      can_stop: true,
      raw_error: sensitiveFragments.join(' '),
    }),
    {
      state_id: 'preparing_launch',
      label: 'Backend preparation copy',
      progress_pct: 100,
      terminal: false,
      playing: false,
      process_live: true,
      can_stop: true,
    },
  );
  const outcome = launchSessionOutcome({
    reason: 'startup_failed',
    kind: 'failed',
    summary: 'Backend terminal summary.',
    raw_error: sensitiveFragments.join(' '),
  });
  assert.deepEqual(outcome, { reason: 'startup_failed', kind: 'failed', summary: 'Backend terminal summary.' });
  assertExcludesSensitive(outcome);
  assert.equal(launchSessionOutcome({ reason: 'startup_failed', kind: 'fatal', summary: 'No' }), undefined);
  assert.equal(launchSessionOutcome({ reason: 'made_up_reason', kind: 'failed', summary: 'No' }), undefined);
});

test('launch status parsing is atomic and terminality remains backend-authored', () => {
  const recovery = launchStatusUpdate(
    launchWireStatus('terminal-session', 2, launchViewModel('recovering', 'Recovering startup')),
    'terminal-session',
  );
  assert.equal(recovery?.viewModel.terminal, false);

  const rawTerminal = launchWireStatus(
    'terminal-session',
    3,
    launchViewModel('failed', 'Guardian is still settling', { progress_pct: 100 }),
  );
  rawTerminal.state = 'failed';
  assert.equal(launchStatusUpdate(rawTerminal, 'terminal-session')?.viewModel.terminal, false);

  const terminal = launchStatusUpdate(
    launchWireStatus(
      'terminal-session',
      4,
      launchViewModel('failed', 'Launch failed', { progress_pct: 100, terminal: true }),
      { reason: 'startup_failed', kind: 'failed', summary: 'Backend terminal summary.' },
    ),
    'terminal-session',
  );
  assert.equal(terminal?.viewModel.terminal, true);
  assert.equal(terminal?.outcome?.summary, 'Backend terminal summary.');

  const malformedHigherRevision = launchWireStatus('terminal-session', 5, {
    ...launchViewModel('exited', 'Exited', { terminal: true }),
    can_stop: undefined,
  });
  assert.equal(launchStatusUpdate(malformedHigherRevision, 'terminal-session'), null);
  assert.equal(launchStatusUpdate(launchWireStatus('other-session', 6, terminal!.viewModel), 'terminal-session'), null);

  const session: LaunchSession = {
    sessionId: 'terminal-session',
    launchedAt: '2026-07-17T00:00:00.000Z',
    viewModel: terminal!.viewModel,
    statusRevision: 1,
  };
  launchSessions.value = { 'instance-a': session };
  assert.equal(endSessionIfCurrent('instance-a', 'terminal-session'), true);
  assert.equal(endSessionIfCurrent('instance-a', 'terminal-session'), false);
  assert.equal(launchSessions.value['instance-a'], undefined);
});

test('launch session presentation distinguishes active recovery from a live process', () => {
  const session = (state: string, viewModel: LaunchStatusViewModel): LaunchSession => ({
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

test('launch session convergence rejects stale, malformed, and replacement-session updates atomically', () => {
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
        { reason: 'clean_exit', kind: 'clean', summary: 'Minecraft closed normally.' },
      ),
    ),
    null,
  );
  assert.equal(
    convergeLaunchStatus(
      'instance-a',
      'ordered-session',
      launchWireStatus('ordered-session', 8, {
        ...launchViewModel('recovering', 'Recovering startup'),
        can_stop: undefined,
      }),
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
      { message: 'Guardian completed recovery.', tone: 'success' },
    ),
  );
  assert.equal(running?.notice?.message, 'Guardian completed recovery.');
  assert.equal(launchSessions.value['instance-a']?.viewModel.playing, true);
  launchSessions.value = {};
});

test('native launch bridge failure closes native listeners but preserves polling convergence', async () => {
  const closed: string[] = [];
  const handle = (name: string): LaunchLiveHandle => ({
    close(): void {
      closed.push(name);
    },
  });
  const transport = await establishNativeLaunchTransport({
    startPoll: () => handle('poll'),
    subscribeStatus: async () => handle('status'),
    subscribeLog: async () => handle('log'),
    startBridge: async () => false,
  });

  assert.deepEqual(closed, ['status', 'log']);
  transport.close();
  assert.deepEqual(closed, ['status', 'log', 'poll']);
});

test('install failure adapters cover every backend state and action without reading raw fields', () => {
  const states = [
    ['failed', true],
    ['failed_retryable', true],
    ['failed_blocked', false],
    ['failed_suppressed', false],
    ['failed_guardian_recorded', true],
    ['failed_instance_removed', false],
  ] as const;

  for (const [stateId, retryEnabled] of states) {
    const fixture = failureFixture(stateId, retryEnabled);
    const view = installFailureViewModel(fixture);
    assert.ok(view);
    assert.equal(view.state_id, stateId);
    assert.equal(view.summary, fixture.summary);
    assert.equal(view.detail, fixture.detail);
    assert.deepEqual(view.details, fixture.details);
    assert.equal(view.retry_action.label, fixture.retry_action.label);
    assert.equal(view.retry_action.enabled, retryEnabled);
    assert.equal(view.retry_action.disabled_reason, fixture.retry_action.disabled_reason);
    assert.equal(view.dismiss_action.enabled, true);
    assertExcludesSensitive(view);
  }
});

test('unresolved install failures use fixed safe copy instead of raw transport, path, token, or stack detail', () => {
  const raw = sensitiveFragments.join('\n');
  const view = unresolvedFailureViewModel(raw);
  assert.equal(view.state_id, 'failure_details_unavailable');
  assert.equal(view.summary, 'Install failed before Axial received safe error details.');
  assert.equal(view.retry_action.enabled, false);
  assert.equal(view.dismiss_action.enabled, true);
  assertExcludesSensitive(view);
});

test('install queue notices render backend copy and backend action tone for every current notice state', () => {
  const fixtures: Array<[string, string, 'success' | 'error' | 'info']> = [
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

test('install failure dismissal and item transitions are scoped to the matching install', () => {
  clearDownloadFailure();
  const item: InstallItem = { versionId: '1.21.6' };
  const other: InstallItem = { versionId: '1.20.1' };
  const view = installFailureViewModel(failureFixture('failed_suppressed', false));
  assert.ok(view);

  recordDownloadFailure(item, 'Minecraft 1.21.6', view);
  assert.equal(downloadFailure.value?.viewModel, view);
  clearDownloadFailureForItem(other);
  assert.equal(downloadFailure.value?.viewModel, view);
  clearDownloadFailureForItem(item);
  assert.equal(downloadFailure.value, null);

  recordDownloadFailure(item, 'Minecraft 1.21.6', view);
  clearDownloadFailure();
  assert.equal(downloadFailure.value, null);
});

test('create surfaces preserve backend Guardian copy and map every rendered notice tone', () => {
  const createResult: CreateResultPresentationSource & Record<string, unknown> = {
    view_model: { summary: 'Instance created.', detail: 'Install queued.' },
    guardian_notice: { message: 'Guardian adjusted the preset.', detail: 'Automatic preset selected.' },
    raw_error: sensitiveFragments.join(' '),
  };
  assert.equal(
    createResultToastMessage(createResult),
    'Instance created. Guardian adjusted the preset. Install queued. Automatic preset selected.',
  );
  assert.equal(createToastKind('error'), 'error');
  assert.equal(createToastKind('warn'), 'info');
  assert.equal(createToastKind('intervened'), 'success');

  const tones = {
    info: ['info', 'info'],
    warn: ['warned', 'alert'],
    warned: ['warned', 'alert'],
    error: ['error', 'alert'],
    intervened: ['intervened', 'shield-check'],
    success: ['success', 'check-circle'],
  } as const;
  for (const [tone, [normalized, icon]] of Object.entries(tones)) {
    const notice: CreateNotice = { state_id: `notice-${tone}`, tone, message: `Backend ${tone} copy` };
    assert.deepEqual(createNoticePresentation(notice), { tone: normalized, icon });
  }
});

test('performance health, Guardian settings, and proof evidence remain backend-authored display contracts', () => {
  const warningHealth: PerformanceHealthFixture = {
    health: 'healthy',
    view_model: {
      tone: 'warn',
      title: 'Backend performance warning',
      detail: 'Backend performance detail',
    },
    raw_error: sensitiveFragments.join(' '),
  };
  assert.deepEqual(performanceHealthNotice(warningHealth), {
    tone: 'warned',
    title: 'Backend performance warning',
    detail: 'Backend performance detail',
  });
  assert.equal(performanceHealthNotice({ view_model: { tone: 'ok' } }), null);

  assert.equal(guardianModeFrom('managed'), 'managed');
  assert.equal(guardianModeFrom('custom'), 'custom');
  assert.equal(guardianModeFrom('disabled'), 'managed');
  assert.deepEqual(
    GUARDIAN_OPTIONS.map(({ value, label }) => [value, label]),
    [
      ['managed', 'Managed'],
      ['custom', 'Custom'],
    ],
  );

  const proofRecord: LaunchProofFixture = {
    guardian: { message: sensitiveFragments.join(' ') },
    healing: { warnings: [sensitiveFragments.join(' ')] },
    view_model: {
      evidence: { tone: 'warn', label: 'Guardian intervened', detail: 'Backend sanitized proof detail' },
    },
  };
  const evidence = launchProofGuardianEvidence(proofRecord);
  assert.deepEqual(evidence, {
    tone: 'warn',
    label: 'Guardian intervened',
    detail: 'Backend sanitized proof detail',
  });
  assertExcludesSensitive(evidence);
});
