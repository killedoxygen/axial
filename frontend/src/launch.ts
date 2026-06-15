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
import { config, launchState, runningSessions, selectedInstance, instanceLaunchDrafts } from './store';
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
import type { Config, Instance, LaunchNotice, LaunchSessionOutcome } from './types';

const PRE_RESPONSE_STAGE_CAP_PCT = 87;
const LIVE_LAUNCH_UPDATES_UNAVAILABLE = 'Live launch updates are unavailable.';
const PRE_RESPONSE_STAGE_TICKS: Array<{ atMs: number; stage: LaunchStage }> = [
  { atMs: 700, stage: 'preparing' },
  { atMs: 1800, stage: 'prewarming' },
  { atMs: 3400, stage: 'starting' },
  { atMs: 6200, stage: 'monitoring' },
];

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

function launchSessionOutcome(value: unknown): LaunchSessionOutcome | undefined {
  if (!value || typeof value !== 'object') return undefined;
  const candidate = value as Partial<LaunchSessionOutcome>;
  if (
    candidate.kind !== 'clean' &&
    candidate.kind !== 'stopped' &&
    candidate.kind !== 'failed' &&
    candidate.kind !== 'unknown'
  ) {
    return undefined;
  }
  if (typeof candidate.reason !== 'string' || typeof candidate.summary !== 'string') return undefined;
  return candidate as LaunchSessionOutcome;
}

function primaryNoticeDetail(details: string[]): string {
  return details[0] || '';
}

function backendLaunchNotice(value: unknown): LaunchNotice | null {
  if (!value || typeof value !== 'object') return null;
  const candidate = value as Partial<LaunchNotice>;
  if (typeof candidate.message !== 'string' || !candidate.message.trim()) return null;
  if (
    candidate.tone !== 'info' &&
    candidate.tone !== 'success' &&
    candidate.tone !== 'warned' &&
    candidate.tone !== 'intervened' &&
    candidate.tone !== 'error'
  ) {
    return null;
  }
  const details = Array.isArray(candidate.details)
    ? candidate.details.filter((detail): detail is string => typeof detail === 'string' && Boolean(detail.trim()))
    : [];
  const detail =
    typeof candidate.detail === 'string' && candidate.detail.trim() ? candidate.detail : primaryNoticeDetail(details);
  return {
    message: candidate.message,
    detail,
    details,
    tone: candidate.tone,
  };
}

function surfaceBackendLaunchNotice(value: unknown, instanceId: string, instanceName: string): boolean {
  const notice = backendLaunchNotice(value);
  if (!notice) return false;
  for (const detail of notice.details || []) {
    appendLog('system', detail, instanceId, instanceName);
  }
  setLaunchNotice(instanceId, notice);
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
  if (!inst?.launch_action?.launchable) return;
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
      if (!surfaceBackendLaunchNotice(res.notice, inst.id, inst.name)) {
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
    surfaceBackendLaunchNotice(res.notice, inst.id, inst.name);

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
        notice?: LaunchNotice;
      };
      if (!surfaceBackendLaunchNotice(payload.notice, inst.id, inst.name)) showError(payload.error || err.message);
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
    const outcome = launchSessionOutcome(data.outcome);
    if (typeof data.state === 'string') updateLaunchPrepStage(instanceId, data.state);
    if (session && (typeof data.pid === 'number' || data.healing || typeof data.state === 'string' || outcome)) {
      updateRunningSession(instanceId, {
        pid: typeof data.pid === 'number' ? data.pid : session.pid || 0,
        state: typeof data.state === 'string' ? data.state : session.state,
        benchmark: data.benchmark || session.benchmark,
        healing: data.healing || session.healing,
        guardian: data.guardian || session.guardian,
        outcome: outcome || session.outcome,
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
  const exitCode = typeof data.exit_code === 'number' ? data.exit_code : undefined;
  const outcome = launchSessionOutcome(data.outcome) || session.outcome;

  eventSource.close();
  endSession(instanceId);

  if (Object.keys(runningSessions.value).length === 0) Music.unsuppress();
  appendLog(
    'system',
    outcome?.summary ||
      `${instanceName || instanceId} exited${exitCode === undefined ? '' : ` with code ${exitCode}`}.`,
    instanceId,
    instanceName,
  );
  surfaceBackendLaunchNotice(data.notice, instanceId, instanceName);
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
