import { batch } from '@preact/signals';
import {
  instances, versions, config, systemInfo, devMode, catalog,
  selectedInstanceId, lastInstanceId,
  installState, installQueue, installFailure, installEventSource,
  launchState, runningSessions, launchNotices,
  currentPage, searchQuery, sidebarFilter, logLines,
} from './store';
import type {
  Instance, Version, Config, SystemInfo, Catalog,
  RunningSession, InstallItem, Page, LaunchNotice,
} from './types';
import { formatInstallItemLabel } from './install-labels';
import { launchStageView, launchStageViewFrom, type LaunchStage } from './launch-stages';

// ── Selection ──

export function selectInstance(id: string | null): void {
  selectedInstanceId.value = id;
  currentPage.value = 'launcher';
}

// ── Install state transitions ──

const INSTALL_FAILURE_MESSAGE_LIMIT = 220;

function cloneInstallItem(item: InstallItem): InstallItem {
  return item.loader
    ? { versionId: item.versionId, loader: { ...item.loader } }
    : { versionId: item.versionId };
}

function boundedInstallFailureMessage(message: string): string {
  const firstUsefulLine = String(message || '')
    .split(/\r?\n/)
    .map(line => line.trim())
    .find(line => line && !line.startsWith('at '));
  const squashed = (firstUsefulLine || 'Install failed before Croopor received error details.')
    .replace(/\s+/g, ' ')
    .trim();
  if (squashed.length <= INSTALL_FAILURE_MESSAGE_LIMIT) return squashed;
  return `${squashed.slice(0, INSTALL_FAILURE_MESSAGE_LIMIT - 3).trimEnd()}...`;
}

export function enqueueInstall(item: InstallItem): void {
  const active = installState.value;
  if (active.status === 'active' && active.versionId === item.versionId) return;
  if (installQueue.value.some(q => q.versionId === item.versionId)) return;
  installQueue.value = [...installQueue.value, item];
}

export function recordInstallFailure(item: InstallItem, message: string): void {
  installFailure.value = {
    item: cloneInstallItem(item),
    displayName: formatInstallItemLabel(item),
    message: boundedInstallFailureMessage(message),
    failedAt: Date.now(),
  };
}

export function clearInstallFailure(): void {
  installFailure.value = null;
}

export function requeueFailedInstall(): boolean {
  const failure = installFailure.value;
  if (!failure) return false;
  const item = cloneInstallItem(failure.item);
  const active = installState.value;
  const wasIdle = installState.value.status === 'idle';
  batch(() => {
    installFailure.value = null;
    const rest = installQueue.value.filter(q => q.versionId !== item.versionId);
    installQueue.value = active.status === 'active' && active.versionId === item.versionId
      ? rest
      : [item, ...rest];
  });
  return wasIdle;
}

export function startInstall(versionId: string, label = 'Starting...', displayName?: string): void {
  installState.value = { status: 'active', versionId, displayName, pct: 0, label, phase: 'starting', startedAt: Date.now() };
}

export function updateInstallProgress(pct: number, label: string, phase?: string): void {
  const current = installState.value;
  if (current.status !== 'active') return;
  const nextPct = Number.isFinite(pct) ? Math.max(0, Math.min(100, pct)) : current.pct;
  const regressed = nextPct < current.pct;
  installState.value = {
    ...current,
    pct: Math.max(current.pct, nextPct),
    label: regressed ? current.label : label,
    phase: regressed ? current.phase : phase || current.phase,
  };
}

export function completeInstall(): void {
  installState.value = { status: 'idle' };
  if (installEventSource.value) {
    installEventSource.value.close();
    installEventSource.value = null;
  }
}

export function dequeueNextInstall(): InstallItem | null {
  const queue = installQueue.value;
  if (queue.length === 0) return null;
  const [next, ...rest] = queue;
  installQueue.value = rest;
  return next;
}

export function removeQueuedInstallAt(index: number): void {
  const queue = installQueue.value;
  if (!Number.isInteger(index) || index < 0 || index >= queue.length) return;
  installQueue.value = queue.filter((_, i) => i !== index);
}

export function setInstallEventSource(es: { close(): void } | null): void {
  if (installEventSource.value) installEventSource.value.close();
  installEventSource.value = es;
}

// ── Launch state transitions ──

export function startLaunch(instanceId: string): void {
  const stage = launchStageView('queued');
  launchState.value = { status: 'preparing', instanceId, pct: stage.pct, label: stage.label, stage: stage.stage };
}

export function updateLaunchPrep(instanceId: string, pct: number, label: string, stage?: LaunchStage): void {
  const current = launchState.value;
  if (current.status !== 'preparing' || current.instanceId !== instanceId) return;
  launchState.value = {
    status: 'preparing',
    instanceId,
    pct: Math.max(current.pct, Math.max(0, Math.min(100, pct))),
    label,
    stage: stage ?? current.stage,
  };
}

export function updateLaunchPrepStage(instanceId: string, backendState: string): void {
  const current = launchState.value;
  if (current.status !== 'preparing' || current.instanceId !== instanceId) return;
  const view = launchStageViewFrom(backendState);
  if (!view) return;
  launchState.value = {
    status: 'preparing',
    instanceId,
    pct: Math.max(current.pct, view.pct),
    label: view.label,
    stage: view.stage,
  };
}

export function confirmLaunch(instanceId: string, session: RunningSession): void {
  batch(() => {
    launchState.value = { status: 'idle' };
    runningSessions.value = { ...runningSessions.value, [instanceId]: session };
  });
}

export function endLaunchPrep(): void {
  launchState.value = { status: 'idle' };
}

export function endSession(instanceId: string): void {
  const next = { ...runningSessions.value };
  delete next[instanceId];
  runningSessions.value = next;
}

export function updateRunningSessionState(
  instanceId: string,
  patch: Partial<RunningSession>,
): void {
  const current = runningSessions.value[instanceId];
  if (!current) return;
  runningSessions.value = {
    ...runningSessions.value,
    [instanceId]: { ...current, ...patch },
  };
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

// ── Data setters ──

export function setVersions(v: Version[]): void { versions.value = v; }
export function setInstances(i: Instance[]): void { instances.value = i; }
export function setConfig(c: Config): void { config.value = c; }
export function setSystemInfo(s: SystemInfo): void { systemInfo.value = s; }
export function setDevMode(d: boolean): void { devMode.value = d; }
export function setCatalog(c: Catalog | null): void { catalog.value = c; }
export function setLastInstanceId(id: string | null): void { lastInstanceId.value = id; }

// ── UI state setters ──

export function navigate(page: Page): void { currentPage.value = page; }
export function setSearch(q: string): void { searchQuery.value = q; }
export function setFilter(f: string): void { sidebarFilter.value = f; }
export function setLogLines(n: number): void { logLines.value = n; }

// ── Instance mutations ──

export function addInstance(inst: Instance): void {
  instances.value = [...instances.value, inst];
}

export function removeInstance(id: string): void {
  batch(() => {
    instances.value = instances.value.filter(i => i.id !== id);
    if (selectedInstanceId.value === id) selectedInstanceId.value = null;
  });
}

export function updateInstanceInList(updated: Instance): void {
  instances.value = instances.value.map(i => i.id === updated.id ? updated : i);
}
