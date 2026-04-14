import { api, API } from './api';
import { byId } from './dom';
import { Sound } from './sound';
import { Music } from './music';
import { fmtMem, showError, appendLog, errMessage } from './utils';
import { clearLaunchVisualState, startLaunchSequence, endLaunchSequence } from './effects';
import { showConfirm } from './dialogs';
import {
  hasNativeDesktopRuntime, nativeLaunchLogEventName, nativeLaunchStatusEventName,
  onNativeEvent, startNativeLaunchEvents,
} from './native';
import {
  config, launchState, runningSessions, selectedInstance, selectedVersion, systemInfo, instanceLaunchDrafts,
} from './store';
import {
  clearLaunchNotice, confirmLaunch, endLaunchPrep, endSession, setLaunchNotice, startLaunch,
  updateInstanceInList, updateRunningSessionState,
} from './actions';
import type { HealingEvent, LaunchHealingSummary } from './types';

function rollbackLaunch(instanceId: string, animationFrameId: number | null): void {
  if (animationFrameId !== null) cancelAnimationFrame(animationFrameId);
  endSession(instanceId);
  if (Object.keys(runningSessions.value).length === 0) Music.unsuppress();
  clearLaunchVisualState();
  endLaunchPrep();
}

function updateRunningSession(instanceId: string, patch: Partial<import('./types').RunningSession>): void {
  updateRunningSessionState(instanceId, patch);
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

function formatPresetName(preset: string): string {
  switch (preset) {
    case 'smooth':
      return 'Smooth';
    case 'performance':
      return 'Performance';
    case 'ultra_low_latency':
      return 'Ultra Low Latency';
    case 'graalvm':
      return 'GraalVM';
    case 'legacy':
      return 'Legacy';
    case 'legacy_pvp':
      return 'Legacy PvP';
    case 'legacy_heavy':
      return 'Legacy Heavy';
    case '':
    case 'none':
      return 'Auto';
    default:
      return preset.replace(/_/g, ' ').replace(/\b\w/g, (m) => m.toUpperCase());
  }
}

function ensureSentence(text: string): string {
  const trimmed = text.trim();
  if (!trimmed) return '';
  if (/[.!?]$/.test(trimmed)) return trimmed;
  return `${trimmed}.`;
}

function formatHealingDetail(detail: string): string {
  const trimmed = detail.trim();
  if (!trimmed) return '';

  let match = trimmed.match(/^Requested JVM preset "([^"]+)" was downgraded to "([^"]+)" for compatibility$/);
  if (match) {
    return `GC preset changed from ${formatPresetName(match[1])} to ${formatPresetName(match[2])} to match this runtime.`;
  }

  if (trimmed === 'Requested Java override was bypassed in favor of a safer managed runtime') {
    return 'Java override was skipped and the managed runtime was used instead.';
  }

  match = trimmed.match(/^Automatic retry: downgraded JVM preset to "([^"]+)" after startup failure$/);
  if (match) {
    return `Croopor retried startup with the ${formatPresetName(match[1])} GC preset.`;
  }

  if (trimmed === 'Automatic retry: disabled custom GC flags after startup failure') {
    return 'Croopor retried startup without custom GC flags.';
  }

  if (trimmed === 'Automatic retry: switched to managed Java after runtime mismatch') {
    return 'Croopor retried startup with the managed Java runtime.';
  }

  match = trimmed.match(/^Launch recovered automatically after (\d+) retry attempt(?:s)?\.$/);
  if (match) {
    const count = Number(match[1]);
    return `Recovered automatically after ${count} ${count === 1 ? 'retry' : 'retries'}.`;
  }

  match = trimmed.match(/^Reason: (.+)$/);
  if (match) {
    return ensureSentence(`Reason: ${match[1]}`);
  }

  return ensureSentence(trimmed);
}

function healingToastMessage(healing: LaunchHealingSummary): string {
  if (healing.failure_class && (!healing.retry_count || healing.retry_count === 0) && !healing.fallback_applied) {
    if (healing.advanced_overrides) {
      return 'Launch stopped before startup because the manual overrides were not compatible.';
    }
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
      return event.detail ? ensureSentence(event.detail) : 'GC preset was adjusted for compatibility.';
    case 'startup_stalled':
      return 'Launch was stopped because no startup activity was detected.';
    case 'fallback_applied':
      return event.detail ? ensureSentence(event.detail) : 'Croopor retried startup with safer settings.';
    default:
      return event.detail ? ensureSentence(event.detail) : '';
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
    pushUniqueNoticeDetail(details, `Launch recovered automatically after ${healing.retry_count} retry attempt${healing.retry_count > 1 ? 's' : ''}.`);
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

function surfaceHealing(healing: LaunchHealingSummary | undefined, instanceId: string, instanceName: string, showNotice = true): void {
  if (!healing) return;
  for (const detail of healingNoticeDetails(healing)) {
    appendLog('system', detail, instanceId, instanceName);
  }
  if (!showNotice) {
    return;
  }
  const message = healingToastMessage(healing);
  if (message) {
    const details = healingNoticeDetails(healing);
    setLaunchNotice(instanceId, {
      message,
      detail: primaryNoticeDetail(details),
      details,
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
      client_started_at_ms: Date.now(),
    });

    if (res.error) {
      surfaceHealing(res.healing, inst.id, inst.name, !res.healing?.failure_class);
      if (!res.healing?.failure_class) {
        showError(res.error);
      } else {
        const detail = friendlyLaunchErrorDetail(res.error);
        const healingDetails = healingNoticeDetails(res.healing || {});
        const details = [detail, ...healingDetails.filter((entry) => entry !== detail)];
        const message = healingToastMessage(res.healing || {}) || 'Launch stopped before startup.';
        setLaunchNotice(inst.id, {
          message,
          detail,
          details,
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
      pid: typeof res.pid === 'number' ? res.pid : 0,
      state: 'queued',
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

  timerId = window.setInterval(() => { void poll(); }, 1000);
  void poll();
  return handle;
}

async function connectLaunchEvents(sessionId: string, instanceId: string, instanceName: string): Promise<void> {
  const onStatus = (data: any, handle: { close(): void }): void => {
    if (runningSessions.value[instanceId]?.sessionId !== sessionId) return;
    if (typeof data.pid === 'number' || data.healing || typeof data.state === 'string') {
      updateRunningSession(instanceId, {
        pid: typeof data.pid === 'number' ? data.pid : runningSessions.value[instanceId]?.pid || 0,
        state: typeof data.state === 'string' ? data.state : runningSessions.value[instanceId]?.state,
        healing: data.healing || runningSessions.value[instanceId]?.healing,
      });
    }
    if (data.state === 'exited') onGameExited(data, instanceId, instanceName, sessionId, handle);
  };

  const onLog = (data: any): void => {
    if (runningSessions.value[instanceId]?.sessionId !== sessionId) return;
    appendLog(data.source, data.text, instanceId, instanceName);
  };

  if (hasNativeDesktopRuntime()) {
    let streamHandle: { close(): void };
    const statusSubscription = await onNativeEvent(nativeLaunchStatusEventName(sessionId), (data) => {
      onStatus(data, streamHandle);
    });
    const logSubscription = await onNativeEvent(nativeLaunchLogEventName(sessionId), onLog);
    if (!statusSubscription || !logSubscription) {
      throw new Error('native launch stream unavailable');
    }
    const pollSubscription = makeLaunchStatusPoller(sessionId, instanceId, onStatus);
    streamHandle = makeCompositeSubscription(statusSubscription, logSubscription, pollSubscription);
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
      message: 'Startup failed and the launch was stopped cleanly.',
      detail: formatHealingDetail(`Reason: ${describeFailureClass(data.failure_class)}`),
      tone: 'error',
    });
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
      showError(`Failed to kill: ${res.error}`);
      return;
    }
  } catch (err: unknown) {
    updateRunningSessionState(inst.id, { stopping: false });
    showError(`Failed to kill: ${errMessage(err)}`);
  }
}
