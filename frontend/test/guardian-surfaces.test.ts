import assert from 'node:assert/strict';
import test from 'node:test';

import { clearLaunchNotice, confirmLaunch, setLaunchNotice, updateRunningSessionState } from '../src/actions';
import { createResultToastMessage, createToastKind } from '../src/instance-create';
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
import { backendLaunchNotice, launchSessionOutcome, launchStatusViewModel } from '../src/launch';
import { launchNotices, runningSessions } from '../src/store';
import { startupWarningMessages } from '../src/startup-warnings';
import type { GuardianSummary } from '../src/types-guardian';
import type { InstallFailureViewModel, InstallItem, InstallQueueNoticeViewModel } from '../src/types-install';
import type { LaunchHealingSummary, LaunchNotice, RunningSession } from '../src/types-launch';
import type { PerformanceHealthResponse } from '../src/types-performance';
import { createNoticePresentation } from '../src/views/create/CreateView';
import { launchActionPresentation, launchNoticePresentation } from '../src/views/instance/components/launch';
import { performanceHealthNotice } from '../src/views/instance/performance-mode';
import { launchProofGuardianEvidence } from '../src/views/settings/PerformanceLabProofHistory';
import { GUARDIAN_OPTIONS, guardianModeFrom } from '../src/views/settings/PerformanceSection';

const sensitiveFragments = [
  '/home/alice/.axial',
  'C:\\Users\\Alice\\AppData',
  'raw-secret-token',
  '--accessToken',
  'java.exe',
  'at installWorker',
];

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
    installQueuedView: { title: 'Retry queued', summary: 'Backend queue summary.' } as never,
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

test('Guardian and Healing session evidence survives status patches while notices dismiss independently', () => {
  runningSessions.value = {};
  launchNotices.value = {};
  const guardian: GuardianSummary = {
    mode: 'managed',
    decision: 'intervened',
    message: 'Guardian adjusted the launch.',
    details: ['Backend Guardian detail.'],
  };
  const healing: LaunchHealingSummary = {
    fallback_applied: 'safe_runtime',
    warnings: ['Backend Healing detail.'],
    retry_count: 1,
  };
  const session: RunningSession = {
    sessionId: 'session-a',
    versionId: '1.21.6',
    pid: 0,
    state: 'queued',
    launchedAt: '2026-07-17T00:00:00.000Z',
    allocatedMB: 4096,
    guardian,
    healing,
  };

  confirmLaunch('instance-a', session);
  updateRunningSessionState('instance-a', { state: 'running', pid: 1234 });
  assert.equal(runningSessions.value['instance-a']?.guardian, guardian);
  assert.equal(runningSessions.value['instance-a']?.healing, healing);

  const first: LaunchNotice = { message: 'First backend notice', tone: 'warned' };
  const second: LaunchNotice = { message: 'Second backend notice', tone: 'intervened' };
  setLaunchNotice('instance-a', first);
  setLaunchNotice('instance-b', second);
  clearLaunchNotice('instance-a');
  assert.equal(launchNotices.value['instance-a'], undefined);
  assert.equal(launchNotices.value['instance-b'], second);

  runningSessions.value = {};
  launchNotices.value = {};
});

test('launch status and outcome adapters accept only bounded typed display fields', () => {
  assert.deepEqual(
    launchStatusViewModel({
      state_id: 'preparing_launch',
      label: 'Backend preparation copy',
      progress_pct: 140,
      terminal: false,
      raw_error: sensitiveFragments.join(' '),
    }),
    { state_id: 'preparing_launch', label: 'Backend preparation copy', progress_pct: 100, terminal: false },
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
  assert.equal(
    createResultToastMessage({
      view_model: { summary: 'Instance created.', detail: 'Install queued.' },
      guardian_notice: { message: 'Guardian adjusted the preset.', detail: 'Automatic preset selected.' },
      raw_error: sensitiveFragments.join(' '),
    } as never),
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
    assert.deepEqual(
      createNoticePresentation({ state_id: `notice-${tone}`, tone, message: `Backend ${tone} copy` } as never),
      { tone: normalized, icon },
    );
  }
});

test('performance health, Guardian settings, and proof evidence remain backend-authored display contracts', () => {
  const warningHealth = {
    health: 'degraded',
    view_model: {
      tone: 'warn',
      title: 'Backend performance warning',
      detail: 'Backend performance detail',
      raw_error: sensitiveFragments.join(' '),
    },
  } as unknown as PerformanceHealthResponse;
  assert.deepEqual(performanceHealthNotice(warningHealth), {
    tone: 'warned',
    title: 'Backend performance warning',
    detail: 'Backend performance detail',
  });
  assert.equal(
    performanceHealthNotice({ health: 'healthy', view_model: { tone: 'ok' } } as unknown as PerformanceHealthResponse),
    null,
  );

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

  const evidence = launchProofGuardianEvidence({
    guardian: { message: sensitiveFragments.join(' ') },
    healing: { warnings: [sensitiveFragments.join(' ')] },
    view_model: {
      evidence: { tone: 'warn', label: 'Guardian intervened', detail: 'Backend sanitized proof detail' },
    },
  } as never);
  assert.deepEqual(evidence, {
    tone: 'warn',
    label: 'Guardian intervened',
    detail: 'Backend sanitized proof detail',
  });
  assertExcludesSensitive(evidence);
});
