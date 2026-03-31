import { api, API } from './api';
import { byId } from './dom';
import { Sound } from './sound';
import { Music } from './music';
import { fmtMem, showError, appendLog } from './utils';
import { clearLaunchVisualState, startLaunchSequence, endLaunchSequence } from './effects';
import { showConfirm } from './dialogs';
import {
  isWailsRuntime, nativeLaunchLogEventName, nativeLaunchStatusEventName,
  onNativeEvent, startNativeLaunchEvents,
} from './native';
import {
  config, launchState, runningSessions, selectedInstance, selectedVersion, systemInfo,
} from './store';
import {
  confirmLaunch, endLaunchPrep, endSession, startLaunch, updateInstanceInList,
} from './actions';

function resetFailedLaunch(instanceId: string): void {
  endSession(instanceId);
  if (Object.keys(runningSessions.value).length === 0) Music.unsuppress();
  clearLaunchVisualState();
  endLaunchPrep();
}

export async function launchGame(): Promise<void> {
  const inst = selectedInstance.value;
  const version = selectedVersion.value;
  if (!inst || !version?.launchable) return;
  if (runningSessions.value[inst.id]) return;
  if (launchState.value.status === 'preparing') return;

  const username = byId<HTMLInputElement>('username-input')?.value.trim() || 'Player';
  const maxMemMB = Math.round(parseFloat(byId<HTMLInputElement>('memory-slider')?.value || '4') * 1024);

  const activeSessions = Object.values(runningSessions.value);
  if (activeSessions.length > 0) {
    const totalMB = systemInfo.value?.total_memory_mb || 0;
    const allocatedMB = activeSessions.reduce((sum, session) => sum + (session.allocatedMB || 0), 0);
    if (totalMB > 0 && allocatedMB + maxMemMB > totalMB - 2048) {
      const ok = await showConfirm(
        `You have ${activeSessions.length} instance${activeSessions.length > 1 ? 's' : ''} running, using ~${fmtMem(allocatedMB / 1024)} of ${fmtMem(totalMB / 1024)} system RAM.\n\nLaunching with ${fmtMem(maxMemMB / 1024)} allocated may cause performance issues.`,
        { confirmText: 'Launch Anyway' },
      );
      if (!ok) return;
    }
  }

  Sound.init();
  clearLaunchVisualState();
  startLaunch(inst.id);
  requestAnimationFrame(() => startLaunchSequence());

  let launchCommitted = false;

  try {
    const res = await api('POST', '/launch', {
      instance_id: inst.id,
      username,
      max_memory_mb: maxMemMB,
    });

    if (res.error) {
      showError(res.error);
      clearLaunchVisualState();
      endLaunchPrep();
      return;
    }

    const launchedAt = res.launched_at || new Date().toISOString();
    confirmLaunch(inst.id, {
      sessionId: res.session_id,
      versionId: inst.version_id,
      pid: res.pid,
      launchedAt,
      allocatedMB: maxMemMB,
    });
    launchCommitted = true;
    endLaunchSequence();
    Music.suppress();
    Sound.ui('launchSuccess');
    byId<HTMLElement>('log-panel')?.classList.add('expanded');
    try {
      await connectLaunchEvents(res.session_id, inst.id, inst.name);
    } catch (err: unknown) {
      showError(`Game launched, but live updates failed: ${(err as Error).message}`);
      appendLog('system', `Live updates unavailable for ${inst.name}; stop detection may be delayed.`, inst.id, inst.name);
    }

    updateInstanceInList({ ...inst, last_played_at: launchedAt });
    if (config.value) {
      config.value = {
        ...config.value,
        username,
        max_memory_mb: maxMemMB,
      };
    }
  } catch (err: unknown) {
    showError((err as Error).message);
    if (!launchCommitted) resetFailedLaunch(inst.id);
  }
}

function makeCompositeSubscription(...subscriptions: Array<{ close(): void } | null>): { close(): void } {
  return {
    close(): void {
      subscriptions.forEach((subscription) => subscription?.close());
    },
  };
}

async function connectLaunchEvents(sessionId: string, instanceId: string, instanceName: string): Promise<void> {
  const onStatus = (data: any, handle: { close(): void }): void => {
    if (runningSessions.value[instanceId]?.sessionId !== sessionId) return;
    if (data.state === 'exited') onGameExited(data.exit_code, instanceId, instanceName, sessionId, handle);
  };

  const onLog = (data: any): void => {
    if (runningSessions.value[instanceId]?.sessionId !== sessionId) return;
    appendLog(data.source, data.text, instanceId, instanceName);
  };

  if (isWailsRuntime()) {
    let streamHandle: { close(): void };
    const statusSubscription = onNativeEvent(nativeLaunchStatusEventName(sessionId), (data) => {
      onStatus(data, streamHandle);
    });
    const logSubscription = onNativeEvent(nativeLaunchLogEventName(sessionId), onLog);
    streamHandle = makeCompositeSubscription(statusSubscription, logSubscription);
    try {
      await startNativeLaunchEvents(sessionId);
    } catch (err: unknown) {
      streamHandle.close();
      throw err;
    }
    return;
  }

  const es = new EventSource(`${API}/launch/${sessionId}/events`);
  es.addEventListener('status', (e: MessageEvent) => {
    onStatus(JSON.parse(e.data), es);
  });

  es.addEventListener('log', (e: MessageEvent) => {
    onLog(JSON.parse(e.data));
  });

  es.onerror = () => {
    if (es.readyState !== EventSource.CLOSED) return;
    if (runningSessions.value[instanceId]?.sessionId !== sessionId) return;
    appendLog('system', `Lost live updates for ${instanceName || instanceId}. The game may still be running.`, instanceId, instanceName);
    es.close();
  };
}

function onGameExited(exitCode: number, instanceId: string, instanceName: string, sessionId: string, eventSource: { close(): void }): void {
  const session = runningSessions.value[instanceId];
  if (!session || session.sessionId !== sessionId) return;

  eventSource.close();
  endSession(instanceId);

  if (Object.keys(runningSessions.value).length === 0) Music.unsuppress();
  if (selectedInstance.value?.id === instanceId) clearLaunchVisualState();

  appendLog('system', `${instanceName || instanceId} exited with code ${exitCode}`, instanceId, instanceName);
}

export async function killGame(): Promise<void> {
  const inst = selectedInstance.value;
  if (!inst) return;
  const session = runningSessions.value[inst.id];
  if (!session) return;

  try {
    await api('POST', `/launch/${session.sessionId}/kill`);
  } catch (err: unknown) {
    showError(`Failed to kill: ${(err as Error).message}`);
  }
}
