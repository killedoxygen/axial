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
  updateLaunchPrepView,
  updateRunningSessionState,
} from './actions';
import type { LaunchNotice } from './types-launch';
import type { Instance } from './types-instance';
import type { Config } from './types-settings';
import { createBackendLaunchNoticeTracker, type BackendLaunchNoticeTracker } from './launch-notice-tracker';
import { launchSessionOutcome, launchStatusViewModel } from './launch-response-adapters';

const LIVE_LAUNCH_UPDATES_UNAVAILABLE = 'Live launch updates are unavailable.';

function rollbackLaunch(instanceId: string): void {
  endSession(instanceId);
  if (Object.keys(runningSessions.value).length === 0) Music.unsuppress();

  endLaunchPrep();
}

function updateRunningSession(instanceId: string, patch: Partial<import('./types-launch').RunningSession>): void {
  updateRunningSessionState(instanceId, patch);
}

function surfaceBackendLaunchNotice(
  value: unknown,
  instanceId: string,
  instanceName: string,
  tracker: BackendLaunchNoticeTracker,
): boolean {
  const notice = tracker.consume(value);
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
  const noticeTracker = createBackendLaunchNoticeTracker();

  Sound.init();

  clearLaunchNotice(inst.id);
  startLaunch(inst.id);

  let launchCommitted = false;
  let launchInst = inst;

  try {
    const launchDraft = instanceLaunchDrafts.value[inst.id];
    if (launchDraft?.dirty) {
      updateLaunchPrep(inst.id, 0, 'Saving launch settings');
      const saved = await api('PUT', `/instances/${encodeURIComponent(inst.id)}`, {
        java_path: launchDraft.javaPath.trim(),
        jvm_preset: launchDraft.jvmPreset,
        extra_jvm_args: launchDraft.extraJvmArgs.trim(),
      });
      if (saved.error) {
        setLaunchNotice(inst.id, {
          message: 'Axial could not save the pending launch overrides.',
          detail: saved.error,
          tone: 'error',
        });
        showError(saved.error);
        rollbackLaunch(inst.id);
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

    updateLaunchPrep(inst.id, 0, 'Requesting launch');
    const res = await api('POST', '/launch', {
      instance_id: launchInst.id,
      username,
      client_started_at_ms: Date.now(),
    });

    if (res.error) {
      if (!surfaceBackendLaunchNotice(res.notice, inst.id, inst.name, noticeTracker)) {
        showError(res.error);
      }
      launchCommitted = false;
      rollbackLaunch(inst.id);
      return;
    }
    const initialViewModel = launchStatusViewModel(res.view_model) ?? undefined;
    if (initialViewModel) updateLaunchPrepView(inst.id, initialViewModel);

    const launchedAt = res.launched_at || new Date().toISOString();
    const allocatedMB = selectedLaunchMaxMemoryMB(res, launchInst, cfg);
    confirmLaunch(inst.id, {
      sessionId: res.session_id,
      versionId: launchInst.version_id,
      pid: typeof res.pid === 'number' ? res.pid : 0,
      state: typeof res.state === 'string' ? res.state : 'queued',
      launchedAt,
      allocatedMB,
      viewModel: initialViewModel,
      benchmark: res.benchmark,
      healing: res.healing,
      guardian: res.guardian,
    });
    launchCommitted = true;
    surfaceBackendLaunchNotice(res.notice, inst.id, inst.name, noticeTracker);

    Music.suppress();
    let launchStarted = false;
    try {
      await connectLaunchEvents(res.session_id, inst.id, inst.name, noticeTracker, () => {
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
      if (!surfaceBackendLaunchNotice(payload.notice, inst.id, inst.name, noticeTracker)) {
        showError(payload.error || err.message);
      }
      if (!launchCommitted) rollbackLaunch(inst.id);
      return;
    }
    showError(errMessage(err));
    if (!launchCommitted) rollbackLaunch(inst.id);
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
  noticeTracker: BackendLaunchNoticeTracker,
  onStarted?: () => void,
): Promise<void> {
  const onStatus = (data: any, handle: { close(): void }): void => {
    const session = runningSessions.value[instanceId];
    const prep = launchState.value;
    const matchingPrep = prep.status === 'preparing' && prep.instanceId === instanceId;
    if (session?.sessionId !== sessionId && !matchingPrep) return;
    surfaceBackendLaunchNotice(data.notice, instanceId, instanceName, noticeTracker);
    const outcome = launchSessionOutcome(data.outcome);
    const viewModel = launchStatusViewModel(data.view_model);
    if (viewModel) updateLaunchPrepView(instanceId, viewModel);
    if (session && (typeof data.pid === 'number' || data.healing || typeof data.state === 'string' || outcome)) {
      updateRunningSession(instanceId, {
        pid: typeof data.pid === 'number' ? data.pid : session.pid || 0,
        state: typeof data.state === 'string' ? data.state : session.state,
        viewModel: viewModel || session.viewModel,
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
  let pollSubscription: { close(): void } | null = null;
  const streamHandle = {
    close(): void {
      es.close();
      pollSubscription?.close();
      pollSubscription = null;
    },
  };
  es.addEventListener('status', (e: MessageEvent) => {
    try {
      onStatus(JSON.parse(e.data), streamHandle);
    } catch {
      // Status polling below remains the convergence path for malformed stream events.
    }
  });

  es.addEventListener('log', (e: MessageEvent) => {
    try {
      onLog(JSON.parse(e.data));
    } catch {
      // Ignore malformed log events; launch status polling owns terminal convergence.
    }
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
    streamHandle.close();
  };
  pollSubscription = makeLaunchStatusPoller(sessionId, instanceId, (data) => {
    onStatus(data, streamHandle);
  });
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
  const outcome = launchSessionOutcome(data.outcome) || session.outcome;

  eventSource.close();
  endSession(instanceId);

  if (Object.keys(runningSessions.value).length === 0) Music.unsuppress();
  appendLog('system', outcome?.summary || `${instanceName || instanceId} session ended.`, instanceId, instanceName);
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
