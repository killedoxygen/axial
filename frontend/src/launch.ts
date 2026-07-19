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
import { config, launchSessions, launchState, selectedInstance, instanceLaunchDrafts } from './store';
import {
  clearLaunchNotice,
  confirmLaunch,
  convergeLaunchStatus,
  endLaunchPrep,
  endSession,
  endSessionIfCurrent,
  setLaunchNotice,
  startLaunch,
  updateInstanceInList,
  updateLaunchPrep,
  updateLaunchPrepView,
  updateLaunchSessionState,
} from './actions';
import type { LaunchNotice, LaunchSessionOutcome } from './types-launch';
import { createBackendLaunchNoticeTracker, type BackendLaunchNoticeTracker } from './launch-notice-tracker';
import { launchStatusUpdate } from './launch-response-adapters';
import { establishNativeLaunchTransport } from './launch-live-transport';

function rollbackLaunch(instanceId: string): void {
  endSession(instanceId);
  if (Object.keys(launchSessions.value).length === 0) Music.unsuppress();

  endLaunchPrep();
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

export async function launchGame(): Promise<void> {
  const inst = selectedInstance.value;
  if (!inst?.launch_action?.launchable) return;
  if (launchSessions.value[inst.id]) return;
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
    const initialStatus = launchStatusUpdate(res, res.session_id);
    if (!initialStatus) throw new Error('Launch response did not match the status contract.');
    updateLaunchPrepView(inst.id, initialStatus.viewModel);

    const launchedAt = res.launched_at || new Date().toISOString();
    confirmLaunch(inst.id, {
      sessionId: res.session_id,
      launchedAt,
      viewModel: initialStatus.viewModel,
      statusRevision: initialStatus.revision,
    });
    launchCommitted = true;
    surfaceBackendLaunchNotice(initialStatus.notice, inst.id, inst.name, noticeTracker);

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
  let inFlight = false;

  const handle = {
    close(): void {
      stopped = true;
      if (timerId) window.clearInterval(timerId);
    },
  };

  const poll = async (): Promise<void> => {
    if (stopped) return;
    if (inFlight) return;
    if (launchSessions.value[instanceId]?.sessionId !== sessionId) {
      handle.close();
      return;
    }
    inFlight = true;
    try {
      const data = await api('GET', `/launch/${sessionId}/status`);
      if (!stopped && !data?.error) onStatus(data, handle);
    } catch {
      // Native events remain primary. Polling is only a convergence fallback.
    } finally {
      inFlight = false;
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
    const session = launchSessions.value[instanceId];
    if (session?.sessionId !== sessionId) return;
    const update = convergeLaunchStatus(instanceId, sessionId, data);
    if (!update) return;
    surfaceBackendLaunchNotice(update.notice, instanceId, instanceName, noticeTracker);
    if (update.viewModel.playing) onStarted?.();
    if (update.viewModel.terminal) {
      onSessionTerminal(update.outcome, instanceId, instanceName, sessionId, handle);
    }
  };

  const onLog = (data: any): void => {
    if (launchSessions.value[instanceId]?.sessionId !== sessionId) return;
    appendLog(data.source, data.text, instanceId, instanceName);
  };

  if (hasNativeDesktopRuntime()) {
    await establishNativeLaunchTransport({
      startPoll: (handle) =>
        makeLaunchStatusPoller(sessionId, instanceId, (data) => {
          onStatus(data, handle);
        }),
      subscribeStatus: (handle) =>
        onNativeEvent(nativeLaunchStatusEventName(sessionId), (data) => {
          onStatus(data, handle);
        }),
      subscribeLog: () => onNativeEvent(nativeLaunchLogEventName(sessionId), onLog),
      startBridge: () => startNativeLaunchEvents(sessionId),
    });
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
    if (launchSessions.value[instanceId]?.sessionId !== sessionId) return;
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

function onSessionTerminal(
  outcome: LaunchSessionOutcome | null,
  instanceId: string,
  instanceName: string,
  sessionId: string,
  eventSource: { close(): void },
): void {
  const session = launchSessions.value[instanceId];
  if (!session || session.sessionId !== sessionId) return;

  if (!endSessionIfCurrent(instanceId, sessionId)) return;
  eventSource.close();

  if (Object.keys(launchSessions.value).length === 0) Music.unsuppress();
  appendLog('system', outcome?.summary || `${instanceName || instanceId} session ended.`, instanceId, instanceName);
}

export async function killGame(): Promise<void> {
  const inst = selectedInstance.value;
  if (!inst) return;
  const session = launchSessions.value[inst.id];
  if (!session) return;
  if (session.stopping) return;
  if (!session.viewModel.can_stop) return;

  try {
    updateLaunchSessionState(inst.id, { stopping: true });
    const res = await api('POST', `/launch/${session.sessionId}/kill`);
    if (res?.error) {
      updateLaunchSessionState(inst.id, { stopping: false });
      showError(`Could not stop the game: ${res.error}`);
      return;
    }
  } catch (err: unknown) {
    updateLaunchSessionState(inst.id, { stopping: false });
    showError(`Could not stop the game: ${errMessage(err)}`);
  }
}
