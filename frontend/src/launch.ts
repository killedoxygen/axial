import { api, apiUrl, isApiError } from './api';
import { Sound } from './sound';
import { Music } from './music';
import { showError, appendLog, errMessage } from './utils';
import {
  hasNativeDesktopRuntime,
  nativeLaunchLogEventName,
  nativeLaunchStatusEventName,
  onNativeEvent,
  startNativeLaunchEvents,
} from './native';
import { config, launchState, runningSessions, selectedInstance, selectedVersion, instanceLaunchDrafts } from './store';
import {
  clearLaunchNotice,
  confirmLaunch,
  endLaunchPrep,
  endSession,
  setLaunchNotice,
  startLaunch,
  updateInstanceInList,
  updateLaunchPrep,
  updateLaunchPrepStage,
  updateRunningSessionState,
} from './actions';
import { launchStageView, type LaunchStage } from './launch-stages';
import type { Config, GuardianSummary, HealingEvent, Instance, LaunchHealingSummary } from './types';

const PRE_RESPONSE_STAGE_CAP_PCT = 87;
const LIVE_LAUNCH_UPDATES_UNAVAILABLE = 'Live launch updates are unavailable.';
const OFFLINE_LAUNCH_AVAILABLE_DETAIL = 'Offline launch remains available for singleplayer and offline-mode servers.';
const PRE_RESPONSE_STAGE_TICKS: Array<{ atMs: number; stage: LaunchStage }> = [
  { atMs: 700, stage: 'preparing' },
  { atMs: 1800, stage: 'prewarming' },
  { atMs: 3400, stage: 'starting' },
  { atMs: 6200, stage: 'monitoring' },
];

interface LaunchAuthFailurePayload {
  error?: string;
  failure_class?: string;
  launch_auth_mode?: string;
  online_mode_ready?: boolean;
  auth_refresh_status?: string;
  auth_refresh_reason?: string;
}

interface LaunchAuthFailureNotice {
  message: string;
  details: string[];
}

function rollbackLaunch(instanceId: string, animationFrameId: number | null): void {
  if (animationFrameId !== null) cancelAnimationFrame(animationFrameId);
  endSession(instanceId);
  if (Object.keys(runningSessions.value).length === 0) Music.unsuppress();

  endLaunchPrep();
}

function startPreResponseLaunchStageTicker(instanceId: string): () => void {
  let stopped = false;
  let timeoutId: number | null = null;
  let nextTick = 0;
  const startedAt = Date.now();

  const scheduleNext = (): void => {
    if (stopped) return;
    const tick = PRE_RESPONSE_STAGE_TICKS[nextTick];
    if (!tick) return;
    const delayMs = Math.max(0, tick.atMs - (Date.now() - startedAt));
    timeoutId = window.setTimeout(() => {
      timeoutId = null;
      if (stopped) return;
      const view = launchStageView(tick.stage);
      updateLaunchPrep(instanceId, Math.min(view.pct, PRE_RESPONSE_STAGE_CAP_PCT), view.label, view.stage);
      nextTick += 1;
      scheduleNext();
    }, delayMs);
  };

  scheduleNext();

  return () => {
    stopped = true;
    if (timeoutId !== null) {
      window.clearTimeout(timeoutId);
      timeoutId = null;
    }
  };
}

function updateRunningSession(instanceId: string, patch: Partial<import('./types').RunningSession>): void {
  updateRunningSessionState(instanceId, patch);
}

function describeFailureClass(failureClass: string | undefined): string {
  switch (failureClass) {
    case 'jvm_unsupported_option':
      return 'unsupported JVM option';
    case 'jvm_experimental_unlock':
      return 'experimental JVM option requires unlock';
    case 'jvm_option_ordering':
      return 'JVM option ordering conflict';
    case 'java_runtime_mismatch':
      return 'Java runtime mismatch';
    case 'classpath_module_conflict':
      return 'classpath or module conflict';
    case 'auth_mode_incompatible':
      return 'auth mode incompatibility';
    case 'loader_bootstrap_failure':
      return 'loader bootstrap failure';
    default:
      return 'startup failure';
  }
}

function ensureSentence(text: string): string {
  const trimmed = text.trim();
  if (!trimmed) return '';
  if (/[.!?]$/.test(trimmed)) return trimmed;
  return `${trimmed}.`;
}

function formatHealingDetail(detail: string): string {
  return ensureSentence(detail);
}

function healingToastMessage(healing: LaunchHealingSummary): string {
  if (healing.failure_class && (!healing.retry_count || healing.retry_count === 0) && !healing.fallback_applied) {
    if (healing.failure_class === 'java_runtime_mismatch') {
      return 'Launch stopped before startup because the required Java runtime was not available.';
    }
    return 'Launch stopped before startup because the selected setup was not compatible.';
  }
  if (healing.retry_count && healing.retry_count > 0) {
    return 'Launch recovered automatically with safer settings.';
  }
  if (healing.fallback_applied || (healing.warnings && healing.warnings.length > 0)) {
    return 'Launch settings were adjusted for compatibility.';
  }
  return '';
}

function formatHealingEvent(event: HealingEvent): string {
  switch (event.kind) {
    case 'runtime_bypassed':
      return 'Java override was skipped and the managed runtime was used instead.';
    case 'preset_downgraded':
      return event.detail || 'GC preset was adjusted for compatibility.';
    case 'fallback_applied':
      return event.detail || 'Croopor retried startup with safer settings.';
    default:
      return event.detail || '';
  }
}

function pushUniqueNoticeDetail(details: string[], detail: string | undefined): void {
  const trimmed = detail ? formatHealingDetail(detail) : '';
  if (!trimmed || details.includes(trimmed)) return;
  details.push(trimmed);
}

function healingNoticeDetails(healing: LaunchHealingSummary): string[] {
  const details: string[] = [];
  for (const event of healing.events || []) {
    pushUniqueNoticeDetail(details, formatHealingEvent(event));
  }
  for (const warning of healing.warnings || []) {
    pushUniqueNoticeDetail(details, warning);
  }
  pushUniqueNoticeDetail(details, healing.fallback_applied);
  if (healing.retry_count && healing.retry_count > 0) {
    pushUniqueNoticeDetail(
      details,
      `Recovered automatically after ${healing.retry_count} ${healing.retry_count === 1 ? 'retry' : 'retries'}.`,
    );
  }
  if (healing.failure_class) {
    pushUniqueNoticeDetail(details, `Reason: ${describeFailureClass(healing.failure_class)}`);
  }
  return details;
}

function primaryNoticeDetail(details: string[]): string {
  return details[0] || '';
}

function friendlyLaunchErrorDetail(message: string): string {
  let detail = message.trim();
  detail = detail.replace(/^resolve healing:\s*/i, '');
  detail = detail.replace(/^explicit /i, 'Manual ');
  if (detail.length > 0) {
    detail = detail.charAt(0).toUpperCase() + detail.slice(1);
  }
  return ensureSentence(detail);
}

function isSelectedOnlineAuthFailure(payload: unknown): payload is LaunchAuthFailurePayload {
  if (!payload || typeof payload !== 'object') return false;
  const candidate = payload as LaunchAuthFailurePayload;
  return (
    candidate.failure_class === 'auth_mode_incompatible' ||
    (candidate.launch_auth_mode === 'online' && candidate.online_mode_ready === false)
  );
}

function compactTokenLabel(value: string | undefined): string {
  return String(value || '')
    .trim()
    .toLowerCase();
}

function isSignInRequiredAuthRefresh(status: string, reason: string): boolean {
  return (
    status === 'sign_in_required' ||
    reason === 'refresh_token_missing' ||
    reason === 'refresh_token_rejected' ||
    reason === 'refresh_state_unavailable'
  );
}

function authSignInRequiredDetail(reason: string): string {
  switch (reason) {
    case 'refresh_token_missing':
      return 'Croopor could not refresh the Microsoft session because the saved sign-in is missing or expired.';
    case 'refresh_token_rejected':
      return 'Microsoft rejected the saved sign-in session.';
    case 'refresh_state_unavailable':
      return 'Croopor could not read the saved sign-in session.';
    default:
      return 'Croopor could not use the saved Microsoft session for Online launch.';
  }
}

function authRefreshFailedDetail(status: string, reason: string): string {
  switch (reason) {
    case 'auth_chain_failed':
      return 'Croopor refreshed Microsoft sign-in, but Minecraft account verification did not complete.';
    case 'client_id_missing':
      return 'Microsoft sign-in is not configured for this build.';
    case 'client_build':
    case 'token_client_unavailable':
      return 'Croopor could not start Microsoft sign-in refresh.';
    case 'oauth_refresh_failed':
    case 'token_endpoint_unreachable':
    case 'token_endpoint_rejected':
    case 'token_endpoint_unavailable':
    case 'token_endpoint_parse_failed':
      return 'Microsoft sign-in refresh is unavailable or did not complete.';
    case 'refreshed_account_unusable':
      return 'The refreshed account could not be used for a verified Minecraft Java launch.';
    default:
      return status === 'refresh_unavailable'
        ? 'Microsoft sign-in refresh is unavailable right now.'
        : 'Croopor could not verify the Microsoft account for Online launch.';
  }
}

export function formatSelectedOnlineAuthFailure(payload: unknown): LaunchAuthFailureNotice | null {
  if (!isSelectedOnlineAuthFailure(payload)) return null;

  const status = compactTokenLabel(payload.auth_refresh_status);
  const reason = compactTokenLabel(payload.auth_refresh_reason);
  if (isSignInRequiredAuthRefresh(status, reason)) {
    return {
      message: 'Online launch needs you to sign in again.',
      details: [
        authSignInRequiredDetail(reason),
        'Sign in again from Accounts, then retry Online launch.',
        OFFLINE_LAUNCH_AVAILABLE_DETAIL,
      ],
    };
  }

  return {
    message: 'Online launch could not verify your Minecraft account.',
    details: [
      authRefreshFailedDetail(status, reason),
      'Refresh or re-verify the account from Accounts, then retry Online launch.',
      OFFLINE_LAUNCH_AVAILABLE_DETAIL,
    ],
  };
}

function guardianNoticeDetails(guardian: GuardianSummary | undefined): string[] {
  if (!guardian) return [];
  if (guardian.details && guardian.details.length > 0) {
    return guardian.details;
  }
  const details: string[] = [];
  for (const intervention of guardian.interventions || []) {
    pushUniqueNoticeDetail(details, intervention.detail);
  }
  for (const guidance of guardian.guidance || []) {
    pushUniqueNoticeDetail(details, guidance);
  }
  return details;
}

function guardianToastMessage(guardian: GuardianSummary | undefined): string {
  if (!guardian) return '';
  if (guardian.message?.trim()) {
    return guardian.message.trim();
  }
  if (guardian.decision === 'blocked') {
    return 'Guardian blocked an unsafe launch setup.';
  }
  if (guardian.decision === 'warned') {
    return 'Guardian found launch settings to review.';
  }
  if (guardian.decision === 'intervened') {
    return 'Guardian adjusted launch settings for safety.';
  }
  return '';
}

function guardianOwnsLaunchOutcome(
  guardian: GuardianSummary | undefined,
  healing: LaunchHealingSummary | undefined,
  noticeDetails = guardianNoticeDetails(guardian),
): boolean {
  if (!guardian) return false;
  if (guardianHasActionableAuthoredDetails(guardian, noticeDetails)) return true;
  if (guardian.decision !== 'intervened') return false;
  if (!healing) return true;
  return !healing.failure_class && !healing.retry_count;
}

function guardianHasAuthoredDetails(
  guardian: GuardianSummary | undefined,
  noticeDetails = guardianNoticeDetails(guardian),
): boolean {
  return Boolean(guardian && noticeDetails.some((detail) => detail.trim()));
}

function guardianHasActionableAuthoredDetails(
  guardian: GuardianSummary | undefined,
  noticeDetails = guardianNoticeDetails(guardian),
): boolean {
  return Boolean(
    guardianHasAuthoredDetails(guardian, noticeDetails) &&
    (guardian?.decision === 'blocked' || guardian?.decision === 'warned' || guardian?.decision === 'intervened'),
  );
}

function guardianOwnsLeadDetail(
  guardian: GuardianSummary | undefined,
  noticeDetails = guardianNoticeDetails(guardian),
): boolean {
  return guardianHasActionableAuthoredDetails(guardian, noticeDetails);
}

function launchOutcomeDetails(
  guardian: GuardianSummary | undefined,
  healing: LaunchHealingSummary | undefined,
  leadDetail = '',
): string[] {
  // Guardian-authored details lead unless Healing owns the concrete failure.
  const details: string[] = [];
  const guardianDetails = guardianNoticeDetails(guardian);
  const hasGuardianAuthoredDetails = guardianHasAuthoredDetails(guardian, guardianDetails);
  const guardianOwnsLead = guardianOwnsLeadDetail(guardian, guardianDetails);
  if (!hasGuardianAuthoredDetails) {
    pushUniqueNoticeDetail(details, leadDetail);
  }
  for (const detail of guardianDetails) {
    pushUniqueNoticeDetail(details, detail);
  }
  if (hasGuardianAuthoredDetails && !guardianOwnsLead) {
    pushUniqueNoticeDetail(details, leadDetail);
  }
  const includeHealing = !guardianOwnsLaunchOutcome(guardian, healing, guardianDetails);
  if (includeHealing) {
    for (const detail of healingNoticeDetails(healing || {})) {
      pushUniqueNoticeDetail(details, detail);
    }
  }
  return details;
}

function launchOutcomeMessage(
  guardian: GuardianSummary | undefined,
  healing: LaunchHealingSummary | undefined,
  fallbackMessage = '',
): string {
  return guardianToastMessage(guardian) || healingToastMessage(healing || {}) || fallbackMessage;
}

function guardianNoticeTone(guardian: GuardianSummary | undefined): import('./types').LaunchNoticeTone | null {
  if (guardian?.decision === 'blocked') return 'error';
  if (guardian?.decision === 'warned') return 'warned';
  if (guardian?.decision === 'intervened') return 'intervened';
  return null;
}

function launchOutcomeTone(
  guardian: GuardianSummary | undefined,
  healing: LaunchHealingSummary | undefined,
): import('./types').LaunchNoticeTone {
  const guardianTone = guardianNoticeTone(guardian);
  if (guardianTone) return guardianTone;
  if (healing?.failure_class) return 'error';
  if (healing?.retry_count && healing.retry_count > 0) return 'success';
  return 'info';
}

function surfaceLaunchOutcome(
  guardian: GuardianSummary | undefined,
  healing: LaunchHealingSummary | undefined,
  instanceId: string,
  instanceName: string,
  showNotice = true,
  leadDetail = '',
  fallbackMessage = '',
): boolean {
  const details = launchOutcomeDetails(guardian, healing, leadDetail);
  for (const detail of details) {
    appendLog('system', detail, instanceId, instanceName);
  }
  if (!showNotice) return details.length > 0;
  const message = launchOutcomeMessage(guardian, healing, fallbackMessage);
  if (!message) return false;
  setLaunchNotice(instanceId, {
    message,
    detail: primaryNoticeDetail(details),
    details,
    tone: launchOutcomeTone(guardian, healing),
  });
  return true;
}

function surfaceSelectedOnlineAuthFailure(payload: unknown, instanceId: string, instanceName: string): boolean {
  const notice = formatSelectedOnlineAuthFailure(payload);
  if (!notice) return false;
  for (const detail of notice.details) {
    appendLog('system', detail, instanceId, instanceName);
  }
  setLaunchNotice(instanceId, {
    message: notice.message,
    detail: primaryNoticeDetail(notice.details),
    details: notice.details,
    tone: 'error',
  });
  return true;
}

function selectedLaunchMaxMemoryMB(response: any, inst: Instance, cfg: Config | null): number {
  if (typeof response?.max_memory_mb === 'number' && response.max_memory_mb > 0) return response.max_memory_mb;
  if (typeof inst.max_memory_mb === 'number' && inst.max_memory_mb > 0) return inst.max_memory_mb;
  if (typeof cfg?.max_memory_mb === 'number' && cfg.max_memory_mb > 0) return cfg.max_memory_mb;
  return 4096;
}

export async function launchGame(): Promise<void> {
  const inst = selectedInstance.value;
  const version = selectedVersion.value;
  if (!inst || !version?.launchable) return;
  if (runningSessions.value[inst.id]) return;
  if (launchState.value.status === 'preparing') return;

  const cfg = config.value;
  const username = cfg?.username || 'Player';

  Sound.init();

  clearLaunchNotice(inst.id);
  startLaunch(inst.id);
  updateLaunchPrep(inst.id, 8, 'Resolving launch plan', 'planning');
  const launchAnimationFrameId: number | null = null;

  let launchCommitted = false;
  let launchInst = inst;

  try {
    updateLaunchPrep(inst.id, 18, 'Checking compatibility', 'validating');
    const launchDraft = instanceLaunchDrafts.value[inst.id];
    if (launchDraft?.dirty) {
      updateLaunchPrep(inst.id, 24, 'Preparing launch files', 'preparing');
      const saved = await api('PUT', `/instances/${encodeURIComponent(inst.id)}`, {
        java_path: launchDraft.javaPath.trim(),
        jvm_preset: launchDraft.jvmPreset,
        extra_jvm_args: launchDraft.extraJvmArgs.trim(),
      });
      if (saved.error) {
        setLaunchNotice(inst.id, {
          message: 'Croopor could not save the pending launch overrides.',
          detail: saved.error,
          tone: 'error',
        });
        showError(saved.error);
        rollbackLaunch(inst.id, launchAnimationFrameId);
        return;
      }
      launchInst = saved;
      updateInstanceInList(saved);
      instanceLaunchDrafts.value = {
        ...instanceLaunchDrafts.value,
        [inst.id]: {
          javaPath: saved.java_path || '',
          jvmPreset: saved.jvm_preset || '',
          extraJvmArgs: saved.extra_jvm_args || '',
          dirty: false,
        },
      };
      appendLog('system', `Applied pending launch overrides for ${inst.name}.`, inst.id, inst.name);
    }

    updateLaunchPrep(inst.id, 46, 'Preparing launch files', 'preparing');
    const stopPreResponseStages = startPreResponseLaunchStageTicker(inst.id);
    const res = await (async () => {
      try {
        return await api('POST', '/launch', {
          instance_id: launchInst.id,
          username,
          client_started_at_ms: Date.now(),
        });
      } finally {
        stopPreResponseStages();
      }
    })();

    if (res.error) {
      const surfaced =
        surfaceSelectedOnlineAuthFailure(res, inst.id, inst.name) ||
        surfaceLaunchOutcome(
          res.guardian,
          res.healing,
          inst.id,
          inst.name,
          true,
          friendlyLaunchErrorDetail(res.error),
          'Launch stopped before startup.',
        );
      if (!surfaced) {
        showError(res.error);
      }
      launchCommitted = false;
      rollbackLaunch(inst.id, launchAnimationFrameId);
      return;
    }

    const launchedAt = res.launched_at || new Date().toISOString();
    const allocatedMB = selectedLaunchMaxMemoryMB(res, launchInst, cfg);
    confirmLaunch(inst.id, {
      sessionId: res.session_id,
      versionId: launchInst.version_id,
      pid: typeof res.pid === 'number' ? res.pid : 0,
      state: typeof res.state === 'string' ? res.state : 'queued',
      launchedAt,
      allocatedMB,
      benchmark: res.benchmark,
      healing: res.healing,
      guardian: res.guardian,
    });
    launchCommitted = true;
    surfaceLaunchOutcome(res.guardian, res.healing, inst.id, inst.name);

    Music.suppress();
    let launchStarted = false;
    try {
      await connectLaunchEvents(res.session_id, inst.id, inst.name, () => {
        if (launchStarted) return;
        launchStarted = true;
        Sound.ui('launchSuccess');
        updateInstanceInList({ ...launchInst, last_played_at: launchedAt });
      });
    } catch (err: unknown) {
      showError(`Launch session started, but live updates failed: ${errMessage(err)}`);
      appendLog(
        'system',
        `Live updates unavailable for ${inst.name}; stop detection may be delayed.`,
        inst.id,
        inst.name,
      );
    }

    if (config.value) {
      config.value = {
        ...config.value,
        username,
      };
    }
  } catch (err: unknown) {
    if (isApiError(err) && err.payload && typeof err.payload === 'object') {
      const payload = err.payload as {
        error?: string;
        guardian?: GuardianSummary;
        healing?: LaunchHealingSummary;
      };
      const surfaced =
        surfaceSelectedOnlineAuthFailure(payload, inst.id, inst.name) ||
        surfaceLaunchOutcome(
          payload.guardian,
          payload.healing,
          inst.id,
          inst.name,
          true,
          friendlyLaunchErrorDetail(payload.error || err.message),
          'Launch stopped before startup.',
        );
      if (!surfaced) showError(payload.error || err.message);
      if (!launchCommitted) rollbackLaunch(inst.id, launchAnimationFrameId);
      return;
    }
    showError(errMessage(err));
    if (!launchCommitted) rollbackLaunch(inst.id, launchAnimationFrameId);
  }
}

function makeLaunchStatusPoller(
  sessionId: string,
  instanceId: string,
  onStatus: (data: any, handle: { close(): void }) => void,
): { close(): void } {
  let stopped = false;
  let timerId = 0;

  const handle = {
    close(): void {
      stopped = true;
      if (timerId) window.clearInterval(timerId);
    },
  };

  const poll = async (): Promise<void> => {
    if (stopped) return;
    if (runningSessions.value[instanceId]?.sessionId !== sessionId) {
      handle.close();
      return;
    }
    try {
      const data = await api('GET', `/launch/${sessionId}/status`);
      if (!stopped && !data?.error) onStatus(data, handle);
    } catch {
      // Native events remain primary. Polling is only a convergence fallback.
    }
  };

  timerId = window.setInterval(() => {
    void poll();
  }, 1000);
  void poll();
  return handle;
}

async function connectLaunchEvents(
  sessionId: string,
  instanceId: string,
  instanceName: string,
  onStarted?: () => void,
): Promise<void> {
  const onStatus = (data: any, handle: { close(): void }): void => {
    const session = runningSessions.value[instanceId];
    const prep = launchState.value;
    const matchingPrep = prep.status === 'preparing' && prep.instanceId === instanceId;
    if (session?.sessionId !== sessionId && !matchingPrep) return;
    if (typeof data.state === 'string') updateLaunchPrepStage(instanceId, data.state);
    if (session && (typeof data.pid === 'number' || data.healing || typeof data.state === 'string')) {
      updateRunningSession(instanceId, {
        pid: typeof data.pid === 'number' ? data.pid : session.pid || 0,
        state: typeof data.state === 'string' ? data.state : session.state,
        benchmark: data.benchmark || session.benchmark,
        healing: data.healing || session.healing,
        guardian: data.guardian || session.guardian,
      });
    }
    if (data.state === 'running') onStarted?.();
    if (data.state === 'exited') onGameExited(data, instanceId, instanceName, sessionId, handle);
  };

  const onLog = (data: any): void => {
    if (runningSessions.value[instanceId]?.sessionId !== sessionId) return;
    appendLog(data.source, data.text, instanceId, instanceName);
  };

  if (hasNativeDesktopRuntime()) {
    let statusSubscription: { close(): void } | null = null;
    let logSubscription: { close(): void } | null = null;
    let pollSubscription: { close(): void } | null = null;
    const streamHandle = {
      close(): void {
        statusSubscription?.close();
        logSubscription?.close();
        pollSubscription?.close();
      },
    };
    statusSubscription = await onNativeEvent(nativeLaunchStatusEventName(sessionId), (data) => {
      onStatus(data, streamHandle);
    });
    logSubscription = await onNativeEvent(nativeLaunchLogEventName(sessionId), onLog);
    if (!statusSubscription || !logSubscription) {
      streamHandle.close();
      throw new Error(LIVE_LAUNCH_UPDATES_UNAVAILABLE);
    }
    pollSubscription = makeLaunchStatusPoller(sessionId, instanceId, (data) => {
      onStatus(data, streamHandle);
    });
    let started = false;
    try {
      started = await startNativeLaunchEvents(sessionId);
    } catch (err: unknown) {
      streamHandle.close();
      throw err;
    }
    if (!started) {
      streamHandle.close();
      throw new Error(LIVE_LAUNCH_UPDATES_UNAVAILABLE);
    }
    return;
  }

  const es = new EventSource(apiUrl(`/launch/${sessionId}/events`));
  es.addEventListener('status', (e: MessageEvent) => {
    onStatus(JSON.parse(e.data), es);
  });

  es.addEventListener('log', (e: MessageEvent) => {
    onLog(JSON.parse(e.data));
  });

  es.onerror = () => {
    if (es.readyState !== EventSource.CLOSED) return;
    if (runningSessions.value[instanceId]?.sessionId !== sessionId) return;
    appendLog(
      'system',
      `Lost live updates for ${instanceName || instanceId}. The game may still be running.`,
      instanceId,
      instanceName,
    );
    es.close();
  };
}

function onGameExited(
  data: any,
  instanceId: string,
  instanceName: string,
  sessionId: string,
  eventSource: { close(): void },
): void {
  const session = runningSessions.value[instanceId];
  if (!session || session.sessionId !== sessionId) return;
  const exitCode = data.exit_code;

  eventSource.close();
  endSession(instanceId);

  if (Object.keys(runningSessions.value).length === 0) Music.unsuppress();
  appendLog('system', `${instanceName || instanceId} exited with code ${exitCode}`, instanceId, instanceName);
  if (typeof data.failure_class === 'string' && data.failure_class) {
    surfaceLaunchOutcome(
      data.guardian,
      data.healing,
      instanceId,
      instanceName,
      true,
      formatHealingDetail(`Reason: ${describeFailureClass(data.failure_class)}`),
      'Startup failed and the launch was stopped cleanly.',
    );
  }
}

export async function killGame(): Promise<void> {
  const inst = selectedInstance.value;
  if (!inst) return;
  const session = runningSessions.value[inst.id];
  if (!session) return;
  if (session.stopping) return;

  try {
    updateRunningSessionState(inst.id, { stopping: true });
    const res = await api('POST', `/launch/${session.sessionId}/kill`);
    if (res?.error) {
      updateRunningSessionState(inst.id, { stopping: false });
      showError(`Could not stop the game: ${res.error}`);
      return;
    }
  } catch (err: unknown) {
    updateRunningSessionState(inst.id, { stopping: false });
    showError(`Could not stop the game: ${errMessage(err)}`);
  }
}
