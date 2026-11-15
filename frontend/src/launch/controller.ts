import { isApiError } from '../platform/transport';
import { Sound } from '../app/sound';
import { Music } from '../music/music';
import { appendLog } from '../app/log';
import { errMessage, showError } from '../app/toast';
import { config } from '../settings/state';
import { instances, selectedInstance, updateInstanceInList } from '../instances/state';
import { updateInstance } from '../instances/api';
import { refreshInstanceReadiness } from '../instances/readiness';
import {
  clearLaunchNotice,
  confirmLaunch,
  convergeLaunchStatus,
  endLaunchPrep,
  endSessionIfCurrent,
  instanceLaunchDrafts,
  launchSessions,
  launchState,
  launchStatusUpdate,
  setLaunchNotice,
  startLaunch,
  updateLaunchPrep,
  updateLaunchPrepView,
  updateLaunchSessionState,
  type LaunchStatusUpdate,
} from './state';
import { createLaunchNoticeTracker, type LaunchNoticeTracker } from './notices';
import { launch, launchIntent, launchRefusal, sessionLogs, sessionStatus, stopSession, subscribeSession } from './api';
import type { LogEntry } from '../generated/LogEntry';
import type { SessionNotice } from '../generated/SessionNotice';
import type { SessionOutcome } from '../generated/SessionOutcome';
import type { SessionSnapshot } from '../generated/SessionSnapshot';

function rollbackLaunch(instanceId: string): void {
  if (launchSessions.value[instanceId]) return;
  if (Object.keys(launchSessions.value).length === 0) Music.unsuppress();

  if (launchState.value.status === 'preparing' && launchState.value.instanceId === instanceId) endLaunchPrep();
}

function surfaceLaunchNotice(notice: SessionNotice | null, instanceId: string, tracker: LaunchNoticeTracker): void {
  const fresh = tracker.consume(notice);
  if (fresh) setLaunchNotice(instanceId, fresh);
}

/** The launch owner says this session is unknown to it. */
function sessionUnknown(error: unknown): boolean {
  return isApiError(error) && error.status === 404 && launchRefusal(error)?.code === 'instance_not_found';
}

export async function launchGame(): Promise<void> {
  const inst = selectedInstance.value;
  if (!inst?.launch_action?.launchable) return;
  if (launchSessions.value[inst.id]) return;
  if (launchState.value.status === 'preparing') return;

  const cfg = config.value;
  const username = cfg?.username;
  const intentKey = crypto.randomUUID();
  const noticeTracker = createLaunchNoticeTracker();

  Sound.init();

  clearLaunchNotice(inst.id);
  startLaunch(inst.id);

  let launchRequested = false;
  let launchInst = inst;

  const acceptLaunch = (snapshot: SessionSnapshot): void => {
    if (snapshot.instance_id !== inst.id) throw new Error('Launch response did not match the requested instance.');
    const sessionId = snapshot.session_id;
    const initialStatus = launchStatusUpdate(snapshot, sessionId);
    if (!initialStatus) throw new Error('Launch response was not a consistent session status.');
    const launchedAt = snapshot.launched_at;
    updateLaunchPrepView(inst.id, initialStatus.viewModel);
    confirmLaunch(inst.id, {
      sessionId,
      intentKey,
      launchedAt,
      viewModel: initialStatus.viewModel,
      statusRevision: initialStatus.revision,
    });
    surfaceLaunchNotice(initialStatus.notice, inst.id, noticeTracker);
    if (initialStatus.viewModel.terminal) {
      onSessionTerminal(initialStatus.outcome, inst.id, inst.name, sessionId, { close() {} });
      return;
    }

    Music.suppress();
    connectLaunchEvents(sessionId, inst.id, inst.name, noticeTracker, () => Sound.ui('launchSuccess'));
  };

  try {
    const launchDraft = instanceLaunchDrafts.value[inst.id];
    if (launchDraft?.dirty) {
      updateLaunchPrep(inst.id, 0, 'Saving launch settings');
      const saved = await updateInstance(inst.id, {
        java_path: launchDraft.javaPath.trim(),
        jvm_preset: launchDraft.jvmPreset,
        extra_jvm_args: launchDraft.extraJvmArgs.trim(),
      });
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
    acceptLaunch(
      await launch({
        instance_id: launchInst.id,
        username,
        intent_key: intentKey,
        client_started_at_ms: Date.now(),
      }),
    );
  } catch (err: unknown) {
    const refused = isApiError(err) && err.status >= 400 && err.status < 500;
    if (launchRequested && !refused && !launchSessions.value[inst.id]) {
      showError('The launch response was interrupted. Checking whether the launch was accepted.');
      updateLaunchPrep(inst.id, 0, 'Checking launch status');
      void recoverLaunchIntent(intentKey, inst.id, inst.name, noticeTracker, acceptLaunch);
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
  noticeTracker: LaunchNoticeTracker,
  acceptLaunch: (snapshot: SessionSnapshot) => void,
): Promise<void> {
  const current = launchState.value;
  if (current.status !== 'preparing' || current.instanceId !== instanceId || launchSessions.value[instanceId]) return;
  try {
    const result = await launchIntent(intentKey);
    if (result.state === 'accepted') {
      acceptLaunch(result.session);
      return;
    }
    if (result.state === 'rejected') {
      showError(result.error || 'The launch request was rejected.');
      rollbackLaunch(instanceId);
      return;
    }
    if (result.state === 'interrupted') {
      showError(result.error || 'The launch was interrupted. Its process outcome is unknown.');
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
  onStatus: (snapshot: SessionSnapshot, handle: { close(): void }) => void,
  onSettled: (update: LaunchStatusUpdate) => void,
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
    if (finishingSessions.has(sessionId)) return;
    const session = launchSessions.value[instanceId];
    if (session?.sessionId !== sessionId) {
      handle.close();
      return;
    }
    inFlight = true;
    try {
      const snapshot = await sessionStatus(sessionId);
      if (!stopped) onStatus(snapshot, handle);
    } catch (error) {
      if (stopped || launchSessions.value[instanceId] !== session || !session.intentKey || !sessionUnknown(error))
        return;
      try {
        const result = await launchIntent(session.intentKey);
        if (stopped || launchSessions.value[instanceId] !== session || result.state !== 'accepted') return;
        const snapshot = result.session;
        if (
          snapshot.instance_id !== instanceId ||
          snapshot.session_id !== sessionId ||
          snapshot.launched_at !== session.launchedAt ||
          snapshot.phase !== 'exited' ||
          snapshot.process_alive !== false ||
          snapshot.stop_allowed !== false ||
          snapshot.tree_settled !== true ||
          snapshot.output_drained !== true
        )
          return;
        const update = launchStatusUpdate(snapshot, sessionId);
        if (
          !update?.viewModel.terminal ||
          update.viewModel.playing ||
          update.viewModel.process_live ||
          update.viewModel.can_stop
        )
          return;
        // Cold settlement snapshots have their own revision. Only the original
        // authenticated terminal intent may bypass live status ordering.
        onSettled(update);
      } catch {
        // Missing, refused or unreadable intent status never proves process exit.
      }
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

function appendSessionLog(entry: LogEntry, sessionId: string, instanceId: string, instanceName: string): void {
  if (launchSessions.value[instanceId]?.sessionId !== sessionId) return;
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

export function reconnectLaunchSession(instanceId: string, instanceName: string): void {
  const session = launchSessions.value[instanceId];
  if (!session) return;
  if (session.viewModel.terminal) return;
  connectLaunchEvents(session.sessionId, instanceId, instanceName, createLaunchNoticeTracker());
}

function connectLaunchEvents(
  sessionId: string,
  instanceId: string,
  instanceName: string,
  noticeTracker: LaunchNoticeTracker,
  onStarted?: () => void,
): void {
  if (launchConnections.has(sessionId)) return;
  let launchStarted = false;
  const onPlaying = (): void => {
    const session = launchSessions.value[instanceId];
    if (launchStarted || session?.sessionId !== sessionId || !session.viewModel.playing) return;
    launchStarted = true;
    const current = instances.value.find((instance) => instance.id === instanceId);
    if (current && (!current.last_played_at || Date.parse(current.last_played_at) < Date.parse(session.launchedAt))) {
      updateInstanceInList({ ...current, last_played_at: session.launchedAt });
    }
    onStarted?.();
  };
  const onStatus = (data: SessionSnapshot, handle: { close(): void }): void => {
    const session = launchSessions.value[instanceId];
    if (session?.sessionId !== sessionId) {
      handle.close();
      return;
    }
    if (finishingSessions.has(sessionId)) return;
    const update = convergeLaunchStatus(instanceId, sessionId, data);
    if (!update) return;
    surfaceLaunchNotice(update.notice, instanceId, noticeTracker);
    if (update.viewModel.playing) onPlaying();
    if (update.viewModel.terminal) {
      onSessionTerminal(update.outcome, instanceId, instanceName, sessionId, handle);
    }
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
  onPlaying();
  // Status is polled only until a stream connection delivers its first
  // authoritative snapshot, and again while that stream is disconnected.
  const startPolling = (): void => {
    if (closed || pollSubscription) return;
    pollSubscription = makeLaunchStatusPoller(
      sessionId,
      instanceId,
      (data) => onStatus(data, streamHandle),
      (update) => {
        updateLaunchSessionState(instanceId, { viewModel: update.viewModel });
        surfaceLaunchNotice(update.notice, instanceId, noticeTracker);
        onSessionTerminal(update.outcome, instanceId, instanceName, sessionId, streamHandle);
      },
    );
  };
  startPolling();
  unsubscribe = subscribeSession(sessionId, {
    status: (snapshot) => {
      if (closed) return;
      // A stream snapshot ends fallback polling only once it proves this exact
      // session incarnation at or beyond the latest state already applied.
      const session = launchSessions.value[instanceId];
      const update = launchStatusUpdate(snapshot, sessionId);
      const converged =
        !!update &&
        session?.sessionId === sessionId &&
        snapshot.instance_id === instanceId &&
        update.launchedAt === session.launchedAt &&
        update.revision >= session.statusRevision;
      onStatus(snapshot, streamHandle);
      if (converged && !closed) {
        pollSubscription?.close();
        pollSubscription = null;
      }
    },
    log: (entry) => {
      if (!closed) appendSessionLog(entry, sessionId, instanceId, instanceName);
    },
    error: () => {
      if (closed || launchSessions.value[instanceId]?.sessionId !== sessionId) return;
      startPolling();
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
  outcome: SessionOutcome | null,
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
        sessionLogs(sessionId),
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
    const result = await stopSession(session.sessionId);
    if (launchSessions.value[inst.id]?.sessionId !== session.sessionId) return;
    if (finishingSessions.has(session.sessionId)) return;
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
    if (finishingSessions.has(session.sessionId)) return;
    updateLaunchSessionState(inst.id, { stopping: false });
    showError(`Could not stop the game: ${errMessage(err)}`);
  }
}
