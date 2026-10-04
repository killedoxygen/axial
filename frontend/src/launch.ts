import { api, isApiError } from './api';
import { subscribeApiEvents } from './backend/events';
import { Sound } from './sound';
import { Music } from './music';
import { showError, appendLog, errMessage } from './utils';
import { config, instances, launchSessions, launchState, selectedInstance, instanceLaunchDrafts } from './store';
import {
  clearLaunchNotice,
  confirmLaunch,
  convergeLaunchStatus,
  endLaunchPrep,
  endSessionIfCurrent,
  setLaunchNotice,
  startLaunch,
  updateInstanceInList,
  updateLaunchPrep,
  updateLaunchPrepView,
  updateLaunchSessionState,
} from './actions';
import type { LaunchSessionOutcome } from './types-launch';
import { createBackendLaunchNoticeTracker, type BackendLaunchNoticeTracker } from './launch-notice-tracker';
import { launchSessionsResponse, launchStatusUpdate } from './launch-response-adapters';
import { dtoEnum, dtoError, dtoRecord, dtoString } from './dto-contract';
import { enrichedInstanceResponse } from './dto-core';
import { launchLogEntryResponse, launchLogsResponse } from './dto-launch';
import { refreshInstanceReadiness } from './instance-readiness';

function rollbackLaunch(instanceId: string): void {
  if (launchSessions.value[instanceId]) return;
  if (Object.keys(launchSessions.value).length === 0) Music.unsuppress();

  if (launchState.value.status === 'preparing' && launchState.value.instanceId === instanceId) endLaunchPrep();
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
  const username = cfg?.username;
  const intentKey = crypto.randomUUID();
  const noticeTracker = createBackendLaunchNoticeTracker();

  Sound.init();

  clearLaunchNotice(inst.id);
  startLaunch(inst.id);

  let launchRequested = false;
  let launchInst = inst;

  const acceptLaunch = (value: unknown): void => {
    const res = dtoRecord(value, 'Launch');
    const sessionId = dtoString(res.session_id, 'Launch session id');
    const initialStatus = launchStatusUpdate(res, sessionId);
    if (!initialStatus) throw new Error('Launch response did not match the status contract.');
    const launchedAt = dtoString(res.launched_at, 'Launch time');
    if (!Number.isFinite(Date.parse(launchedAt)))
      throw new Error('Launch response did not include a valid start time.');
    updateLaunchPrepView(inst.id, initialStatus.viewModel);
    confirmLaunch(inst.id, {
      sessionId,
      launchedAt,
      viewModel: initialStatus.viewModel,
      statusRevision: initialStatus.revision,
    });
    surfaceBackendLaunchNotice(initialStatus.notice, inst.id, inst.name, noticeTracker);
    if (initialStatus.viewModel.terminal) {
      onSessionTerminal(initialStatus.outcome, inst.id, inst.name, sessionId, { close() {} });
      return;
    }

    Music.suppress();
    let launchStarted = false;
    const onStarted = (): void => {
      if (launchStarted) return;
      launchStarted = true;
      Sound.ui('launchSuccess');
      const current = instances.value.find((item) => item.id === inst.id);
      if (current) updateInstanceInList({ ...current, last_played_at: launchedAt });
    };
    if (initialStatus.viewModel.playing) onStarted();
    connectLaunchEvents(sessionId, inst.id, inst.name, noticeTracker, onStarted);
  };

  try {
    const launchDraft = instanceLaunchDrafts.value[inst.id];
    if (launchDraft?.dirty) {
      updateLaunchPrep(inst.id, 0, 'Saving launch settings');
      const saved = enrichedInstanceResponse(
        await api('PUT', `/instances/${encodeURIComponent(inst.id)}`, {
          java_path: launchDraft.javaPath.trim(),
          jvm_preset: launchDraft.jvmPreset,
          extra_jvm_args: launchDraft.extraJvmArgs.trim(),
        }),
      );
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
    launchRequested = true;
    const res = dtoRecord(
      await api('POST', '/launch', {
        instance_id: launchInst.id,
        username,
        intent_key: intentKey,
        client_started_at_ms: Date.now(),
      }),
      'Launch',
    );

    const launchError = dtoError(res);
    if (launchError) {
      if (!surfaceBackendLaunchNotice(res.notice, inst.id, inst.name, noticeTracker)) {
        showError(launchError);
      }
      rollbackLaunch(inst.id);
      return;
    }
    acceptLaunch(res);
  } catch (err: unknown) {
    const refused = isApiError(err) && err.status >= 400 && err.status < 500;
    if (launchRequested && !refused && !launchSessions.value[inst.id]) {
      showError('The launch response was interrupted. Checking whether the launch was accepted.');
      updateLaunchPrep(inst.id, 0, 'Checking launch status');
      void recoverLaunchIntent(intentKey, inst.id, inst.name, noticeTracker, acceptLaunch);
      return;
    }
    if (isApiError(err) && err.payload && typeof err.payload === 'object') {
      const payload = dtoRecord(err.payload, 'Launch error');
      if (!surfaceBackendLaunchNotice(payload.notice, inst.id, inst.name, noticeTracker)) {
        showError(dtoError(payload) || err.message);
      }
      rollbackLaunch(inst.id);
      return;
    }
    showError(errMessage(err));
    rollbackLaunch(inst.id);
  }
}

async function recoverLaunchIntent(
  intentKey: string,
  instanceId: string,
  instanceName: string,
  noticeTracker: BackendLaunchNoticeTracker,
  acceptLaunch: (value: unknown) => void,
): Promise<void> {
  const current = launchState.value;
  if (current.status !== 'preparing' || current.instanceId !== instanceId || launchSessions.value[instanceId]) return;
  try {
    const result = dtoRecord(await api('GET', `/launch/intents/${encodeURIComponent(intentKey)}`), 'Launch intent');
    const state = dtoEnum(result.state, 'Launch intent state', ['preparing', 'accepted', 'rejected', 'interrupted']);
    if (state === 'accepted') {
      acceptLaunch(result.session);
      return;
    }
    if (state === 'rejected') {
      if (!surfaceBackendLaunchNotice(result.notice, instanceId, instanceName, noticeTracker)) {
        showError(dtoError(result) || 'The launch request was rejected.');
      }
      rollbackLaunch(instanceId);
      return;
    }
    if (state === 'interrupted') {
      if (!dtoString(result.session_id, 'Interrupted launch session id').trim()) {
        throw new Error('Interrupted launch session identity was missing.');
      }
      showError(dtoError(result) || 'The launch was interrupted. Its process outcome is unknown.');
      updateLaunchPrep(instanceId, 0, 'Launch interrupted, outcome unknown');
      return;
    }
  } catch {
    // A missing/unreachable intent does not prove that the launch was rejected.
  }
  window.setTimeout(() => {
    void recoverLaunchIntent(intentKey, instanceId, instanceName, noticeTracker, acceptLaunch);
  }, 1000);
}

function makeLaunchStatusPoller(
  sessionId: string,
  instanceId: string,
  onStatus: (data: unknown, handle: { close(): void }) => void,
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
      const data = await api('GET', `/launch/${encodeURIComponent(sessionId)}/status`);
      if (!stopped && !dtoError(data)) onStatus(data, handle);
    } catch {
      // A failed read never implies process exit. The stream and future reads can converge.
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

const launchConnections = new Map<string, { close(): void }>();
const launchLogSequences = new Map<string, number>();
const finishingSessions = new Map<string, Promise<void>>();

function appendSessionLog(value: unknown, sessionId: string, instanceId: string, instanceName: string): void {
  if (launchSessions.value[instanceId]?.sessionId !== sessionId) return;
  const entry = launchLogEntryResponse(value);
  const previous = launchLogSequences.get(sessionId) ?? 0;
  if (entry.sequence <= previous) return;
  if (entry.sequence > previous + 1) {
    appendLog(
      'system',
      'Earlier launch output is no longer available in the retained log history.',
      instanceId,
      instanceName,
    );
  }
  appendLog(entry.source, entry.truncated ? `${entry.text} [truncated]` : entry.text, instanceId, instanceName);
  launchLogSequences.set(sessionId, entry.sequence);
}

export async function adoptLaunchSession(sessionId: string, isCurrent: () => boolean = () => true): Promise<void> {
  if (!isCurrent()) return;
  const sessions = launchSessions.value;
  const preparation = launchState.value;
  const stillCurrent = (): boolean =>
    isCurrent() && launchSessions.value === sessions && launchState.value === preparation;
  try {
    const tracked = Object.entries(sessions).find(([, session]) => session.sessionId === sessionId);
    if (tracked) {
      const [instanceId] = tracked;
      reconnectLaunchSession(
        instanceId,
        instances.value.find((instance) => instance.id === instanceId)?.name ?? instanceId,
      );
      return;
    }
    const value = await api('GET', `/launch/${encodeURIComponent(sessionId)}/status`);
    if (!stillCurrent()) return;
    const snapshot = dtoRecord(value, 'Launch session');
    if (snapshot.session_id !== sessionId) throw new Error('Launch session identity did not match.');
    const entry = Object.entries(launchSessionsResponse({ sessions: [snapshot] }))[0];
    if (!entry) return;
    const [instanceId, session] = entry;
    const instance = instances.value.find((row) => row.id === instanceId);
    if (!instance || sessions[instanceId]) return;
    if (preparation.status === 'preparing' && preparation.instanceId === instanceId) return;
    launchSessions.value = { ...sessions, [instanceId]: session };
    Music.suppress();
    reconnectLaunchSession(instanceId, instance.name);
  } catch {
    if (stillCurrent()) showError('Could not refresh the benchmark game session. Refresh the driver to try again.');
  }
}

export function reconnectLaunchSession(instanceId: string, instanceName: string): void {
  const session = launchSessions.value[instanceId];
  if (!session) return;
  if (session.viewModel.terminal) return;
  connectLaunchEvents(session.sessionId, instanceId, instanceName, createBackendLaunchNoticeTracker());
}

function connectLaunchEvents(
  sessionId: string,
  instanceId: string,
  instanceName: string,
  noticeTracker: BackendLaunchNoticeTracker,
  onStarted?: () => void,
): void {
  if (launchConnections.has(sessionId)) return;
  const onStatus = (data: unknown, handle: { close(): void }): void => {
    const session = launchSessions.value[instanceId];
    if (session?.sessionId !== sessionId) {
      handle.close();
      return;
    }
    const update = convergeLaunchStatus(instanceId, sessionId, data);
    if (!update) return;
    surfaceBackendLaunchNotice(update.notice, instanceId, instanceName, noticeTracker);
    if (update.viewModel.playing) onStarted?.();
    if (update.viewModel.terminal) {
      onSessionTerminal(update.outcome, instanceId, instanceName, sessionId, handle);
    }
  };

  const onLog = (data: unknown): void => {
    appendSessionLog(data, sessionId, instanceId, instanceName);
  };

  let unsubscribe: (() => void) | null = null;
  let pollSubscription: { close(): void } | null = null;
  let closed = false;
  const streamHandle = {
    close(): void {
      closed = true;
      unsubscribe?.();
      unsubscribe = null;
      pollSubscription?.close();
      pollSubscription = null;
      launchConnections.delete(sessionId);
      launchLogSequences.delete(sessionId);
    },
  };
  launchConnections.set(sessionId, streamHandle);
  pollSubscription = makeLaunchStatusPoller(sessionId, instanceId, (data) => {
    onStatus(data, streamHandle);
  });
  unsubscribe = subscribeApiEvents(`/launch/${encodeURIComponent(sessionId)}/events`, {
    decode: (value: unknown) => value,
    events: ['status', 'log'],
    allowLegacyEvents: true,
    onValue: (value, eventName) => {
      if (closed) return;
      if (eventName === 'status') onStatus(value, streamHandle);
      if (eventName === 'log') onLog(value);
    },
    onError: () => {
      if (closed || launchSessions.value[instanceId]?.sessionId !== sessionId) return;
      appendLog(
        'system',
        `Live logs are reconnecting for ${instanceName || instanceId}. Checking session status continues.`,
        instanceId,
        instanceName,
      );
    },
  });
  if (closed) unsubscribe();
}

function onSessionTerminal(
  outcome: LaunchSessionOutcome | null,
  instanceId: string,
  instanceName: string,
  sessionId: string,
  eventSource: { close(): void },
): void {
  const session = launchSessions.value[instanceId];
  if (!session || session.sessionId !== sessionId || finishingSessions.has(sessionId)) return;
  const finishing = Promise.resolve().then(async (): Promise<void> => {
    let timeout: ReturnType<typeof setTimeout> | undefined;
    try {
      const entries = await Promise.race([
        api('GET', `/launch/${encodeURIComponent(sessionId)}/logs`).then(launchLogsResponse),
        new Promise<never>((_resolve, reject) => {
          timeout = setTimeout(() => reject(new Error('Final launch logs timed out.')), 5000);
        }),
      ]);
      for (const entry of entries) appendSessionLog(entry, sessionId, instanceId, instanceName);
    } catch {
      if (launchSessions.value[instanceId]?.sessionId === sessionId) {
        appendLog(
          'system',
          'The session ended, but its final log history could not be refreshed.',
          instanceId,
          instanceName,
        );
      }
    } finally {
      if (timeout !== undefined) clearTimeout(timeout);
      eventSource.close();
      launchLogSequences.delete(sessionId);
      finishingSessions.delete(sessionId);
    }
    if (!endSessionIfCurrent(instanceId, sessionId)) return;
    if (Object.keys(launchSessions.value).length === 0) Music.unsuppress();
    appendLog('system', outcome?.summary || `${instanceName || instanceId} session ended.`, instanceId, instanceName);
    await refreshInstanceReadiness(instanceId);
  });
  finishingSessions.set(sessionId, finishing);
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
    const result = await api('POST', `/launch/${encodeURIComponent(session.sessionId)}/kill`);
    if (launchSessions.value[inst.id]?.sessionId !== session.sessionId) return;
    const error = dtoError(result);
    if (error) {
      updateLaunchSessionState(inst.id, { stopping: false });
      showError(`Could not stop the game: ${error}`);
      return;
    }
    const update = convergeLaunchStatus(inst.id, session.sessionId, result);
    if (update?.viewModel.terminal) {
      onSessionTerminal(
        update.outcome,
        inst.id,
        inst.name,
        session.sessionId,
        launchConnections.get(session.sessionId) ?? { close() {} },
      );
    } else if (update) {
      updateLaunchSessionState(inst.id, { stopping: false });
    }
  } catch (err: unknown) {
    if (launchSessions.value[inst.id]?.sessionId !== session.sessionId) return;
    updateLaunchSessionState(inst.id, { stopping: false });
    showError(`Could not stop the game: ${errMessage(err)}`);
  }
}
