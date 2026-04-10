import { api, API } from './api';
import { byId } from './dom';
import { Sound } from './sound';
import { Music } from './music';
import { fmtMem, showError, appendLog, errMessage } from './utils';
import { clearLaunchVisualState, startLaunchSequence, endLaunchSequence } from './effects';
import { showConfirm } from './dialogs';
import {
  isWailsRuntime, nativeLaunchLogEventName, nativeLaunchStatusEventName,
  onNativeEvent, startNativeLaunchEvents,
} from './native';
import {
  config, launchState, runningSessions, selectedInstance, selectedVersion, systemInfo, instanceLaunchDrafts,
} from './store';
import {
  clearLaunchNotice, confirmLaunch, endLaunchPrep, endSession, setLaunchNotice, startLaunch, updateInstanceInList,
} from './actions';
import type { LaunchHealingSummary } from './types';

function rollbackLaunch(instanceId: string, animationFrameId: number | null): void {
  if (animationFrameId !== null) cancelAnimationFrame(animationFrameId);
  endSession(instanceId);
  if (Object.keys(runningSessions.value).length === 0) Music.unsuppress();
  clearLaunchVisualState();
  endLaunchPrep();
}

function describeFailureClass(failureClass: string | undefined): string {
  switch (failureClass) {
    case 'jvm_unsupported_option':
      return 'unsupported JVM option';
    case 'jvm_experimental_unlock_required':
      return 'experimental JVM option requires unlock';
    case 'jvm_option_ordering':
      return 'JVM option ordering conflict';
    case 'java_runtime_mismatch':
      return 'Java runtime mismatch';
    case 'classpath_or_module_conflict':
      return 'classpath or module conflict';
    case 'auth_mode_incompatible':
      return 'auth mode incompatibility';
    case 'loader_bootstrap_failure':
      return 'loader bootstrap failure';
    default:
      return 'startup failure';
  }
}

function healingToastMessage(healing: LaunchHealingSummary): string {
  if (healing.failure_class && (!healing.retry_count || healing.retry_count === 0) && !healing.fallback_applied) {
    return 'We got you. Croopor stopped this launch before startup because the manual settings were incompatible.';
  }
  if (healing.retry_count && healing.retry_count > 0) {
    return 'We got you. Croopor retried this launch with safer settings.';
  }
  if (healing.fallback_applied || (healing.warnings && healing.warnings.length > 0)) {
    return 'We got you. Croopor adjusted this launch for compatibility.';
  }
  return '';
}

function healingNoticeDetail(healing: LaunchHealingSummary): string {
  if (healing.fallback_applied) return healing.fallback_applied;
  if (healing.warnings && healing.warnings.length > 0) return healing.warnings[0];
  if (healing.failure_class) return `Reason: ${describeFailureClass(healing.failure_class)}`;
  return '';
}

function friendlyLaunchErrorDetail(message: string): string {
  let detail = message.trim();
  detail = detail.replace(/^resolve healing:\s*/i, '');
  detail = detail.replace(/^explicit /i, 'Manual ');
  if (detail.length > 0) {
    detail = detail.charAt(0).toUpperCase() + detail.slice(1);
  }
  return detail;
}

function surfaceHealing(healing: LaunchHealingSummary | undefined, instanceId: string, instanceName: string, showNotice = true): void {
  if (!healing) return;
  for (const warning of healing.warnings || []) {
    appendLog('system', warning, instanceId, instanceName);
  }
  if (healing.fallback_applied) {
    appendLog('system', healing.fallback_applied, instanceId, instanceName);
  }
  if (healing.retry_count && healing.retry_count > 0) {
    appendLog('system', `Launch recovered automatically after ${healing.retry_count} retry attempt${healing.retry_count > 1 ? 's' : ''}.`, instanceId, instanceName);
  }
  if (!showNotice) {
    return;
  }
  const message = healingToastMessage(healing);
  if (message) {
    setLaunchNotice(instanceId, {
      message,
      detail: healingNoticeDetail(healing),
      tone: healing.failure_class ? 'error' : (healing.retry_count && healing.retry_count > 0 ? 'success' : 'info'),
    });
  }
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
  clearLaunchNotice(inst.id);
  startLaunch(inst.id);
  const launchAnimationFrameId = requestAnimationFrame(() => startLaunchSequence());

  let launchCommitted = false;
  let launchInst = inst;

  try {
    const launchDraft = instanceLaunchDrafts.value[inst.id];
    if (launchDraft?.dirty) {
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

    const res = await api('POST', '/launch', {
      instance_id: launchInst.id,
      username,
      max_memory_mb: maxMemMB,
    });

    if (res.error) {
      surfaceHealing(res.healing, inst.id, inst.name, !res.healing?.failure_class);
      if (!res.healing?.failure_class) {
        showError(res.error);
      } else {
        const detail = friendlyLaunchErrorDetail(res.error);
        setLaunchNotice(inst.id, {
          message: 'We got you. Croopor stopped this launch before startup because the manual settings were incompatible.',
          detail,
          tone: 'error',
        });
        appendLog('system', detail, inst.id, inst.name);
      }
      launchCommitted = false;
      rollbackLaunch(inst.id, launchAnimationFrameId);
      return;
    }

    const launchedAt = res.launched_at || new Date().toISOString();
    confirmLaunch(inst.id, {
      sessionId: res.session_id,
      versionId: launchInst.version_id,
      pid: res.pid,
      launchedAt,
      allocatedMB: maxMemMB,
      healing: res.healing,
    });
    launchCommitted = true;
    surfaceHealing(res.healing, inst.id, inst.name);
    endLaunchSequence();
    Music.suppress();
    Sound.ui('launchSuccess');
    try {
      await connectLaunchEvents(res.session_id, inst.id, inst.name);
    } catch (err: unknown) {
      showError(`Game launched, but live updates failed: ${errMessage(err)}`);
      appendLog('system', `Live updates unavailable for ${inst.name}; stop detection may be delayed.`, inst.id, inst.name);
    }

    updateInstanceInList({ ...launchInst, last_played_at: launchedAt });
    if (config.value) {
      config.value = {
        ...config.value,
        username,
        max_memory_mb: maxMemMB,
      };
    }
  } catch (err: unknown) {
    showError(errMessage(err));
    if (!launchCommitted) rollbackLaunch(inst.id, launchAnimationFrameId);
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
    if (data.state === 'exited') onGameExited(data, instanceId, instanceName, sessionId, handle);
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

function onGameExited(data: any, instanceId: string, instanceName: string, sessionId: string, eventSource: { close(): void }): void {
  const session = runningSessions.value[instanceId];
  if (!session || session.sessionId !== sessionId) return;
  const exitCode = data.exit_code;

  eventSource.close();
  endSession(instanceId);

  if (Object.keys(runningSessions.value).length === 0) Music.unsuppress();
  if (selectedInstance.value?.id === instanceId) clearLaunchVisualState();

  appendLog('system', `${instanceName || instanceId} exited with code ${exitCode}`, instanceId, instanceName);
  if (typeof data.failure_class === 'string' && data.failure_class) {
    setLaunchNotice(instanceId, {
      message: 'We got you. Croopor detected a startup failure and stopped the launch cleanly.',
      detail: `Reason: ${describeFailureClass(data.failure_class)}`,
      tone: 'error',
    });
  }
}

export async function killGame(): Promise<void> {
  const inst = selectedInstance.value;
  if (!inst) return;
  const session = runningSessions.value[inst.id];
  if (!session) return;

  try {
    const res = await api('POST', `/launch/${session.sessionId}/kill`);
    if (res?.error) {
      showError(`Failed to kill: ${res.error}`);
      return;
    }
  } catch (err: unknown) {
    showError(`Failed to kill: ${errMessage(err)}`);
  }
}
