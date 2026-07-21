import { batch } from '@preact/signals';
import { instances, config, selectedInstanceId, launchState, launchSessions, launchNotices } from './store';
import type { LaunchSession, LaunchNotice, LaunchStatusUpdate } from './types-launch';
import type { Instance } from './types-instance';
import type { Config } from './types-settings';
import type { LaunchStatusViewModel } from './types-launch';
import { launchStatusUpdate } from './launch-response-adapters';

export function selectInstance(id: string | null): void {
  selectedInstanceId.value = id;
}

export function startLaunch(instanceId: string): void {
  launchState.value = { status: 'preparing', instanceId, pct: 0, label: 'Starting launch', determinate: false };
}

export function updateLaunchPrep(instanceId: string, pct: number, label: string, stage?: string): void {
  const current = launchState.value;
  if (current.status !== 'preparing' || current.instanceId !== instanceId) return;
  const nextPct = Math.max(0, Math.min(100, pct));
  launchState.value = {
    status: 'preparing',
    instanceId,
    pct: Math.max(current.pct, nextPct),
    label,
    stage: stage ?? current.stage,
    determinate: current.determinate === true || nextPct > 0,
  };
}

export function updateLaunchPrepView(instanceId: string, viewModel: LaunchStatusViewModel): void {
  const current = launchState.value;
  if (current.status !== 'preparing' || current.instanceId !== instanceId) return;
  const pct = Number.isFinite(viewModel.progress_pct) ? viewModel.progress_pct : current.pct;
  const label = viewModel.label.trim() || current.label;
  launchState.value = {
    status: 'preparing',
    instanceId,
    pct: Math.max(current.pct, Math.max(0, Math.min(100, pct))),
    label,
    stage: viewModel.state_id || current.stage,
    determinate: true,
  };
}

export function confirmLaunch(instanceId: string, session: LaunchSession): void {
  batch(() => {
    launchState.value = { status: 'idle' };
    launchSessions.value = { ...launchSessions.value, [instanceId]: session };
  });
}

export function endLaunchPrep(): void {
  launchState.value = { status: 'idle' };
}

export function endSession(instanceId: string): void {
  const next = { ...launchSessions.value };
  delete next[instanceId];
  launchSessions.value = next;
}

export function endSessionIfCurrent(instanceId: string, sessionId: string): boolean {
  if (launchSessions.value[instanceId]?.sessionId !== sessionId) return false;
  endSession(instanceId);
  return true;
}

export function updateLaunchSessionState(instanceId: string, patch: Partial<LaunchSession>): void {
  const current = launchSessions.value[instanceId];
  if (!current) return;
  launchSessions.value = {
    ...launchSessions.value,
    [instanceId]: { ...current, ...patch },
  };
}

function applyLaunchStatusUpdate(instanceId: string, sessionId: string, update: LaunchStatusUpdate): boolean {
  const current = launchSessions.value[instanceId];
  if (!current || current.sessionId !== sessionId || update.revision <= current.statusRevision) return false;
  launchSessions.value = {
    ...launchSessions.value,
    [instanceId]: {
      ...current,
      viewModel: update.viewModel,
      statusRevision: update.revision,
    },
  };
  return true;
}

export function convergeLaunchStatus(instanceId: string, sessionId: string, value: unknown): LaunchStatusUpdate | null {
  const update = launchStatusUpdate(value, sessionId);
  if (!update || !applyLaunchStatusUpdate(instanceId, sessionId, update)) return null;
  return update;
}

export function setLaunchNotice(instanceId: string, notice: LaunchNotice): void {
  launchNotices.value = { ...launchNotices.value, [instanceId]: notice };
}

export function clearLaunchNotice(instanceId: string): void {
  if (!launchNotices.value[instanceId]) return;
  const next = { ...launchNotices.value };
  delete next[instanceId];
  launchNotices.value = next;
}

export function setConfig(c: Config): void {
  config.value = c;
}

export function addInstance(inst: Instance): void {
  instances.value = [...instances.value, inst];
}

export function removeInstance(id: string): void {
  batch(() => {
    instances.value = instances.value.filter((i) => i.id !== id);
    if (selectedInstanceId.value === id) selectedInstanceId.value = null;
  });
}

export function updateInstanceInList(updated: Instance): void {
  instances.value = instances.value.map((i) => (i.id === updated.id ? updated : i));
}
