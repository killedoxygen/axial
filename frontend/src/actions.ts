import { batch } from '@preact/signals';
import {
  instances, versions, config, systemInfo, devMode, catalog,
  selectedInstanceId, lastInstanceId,
  installState, installQueue, installEventSource,
  launchState, runningSessions, launchNotices,
  currentPage, searchQuery, sidebarFilter, logLines,
} from './store';
import type {
  Instance, Version, Config, SystemInfo, Catalog,
  RunningSession, InstallItem, Page, LaunchNotice,
} from './types';

// ── Selection ──

export function selectInstance(id: string | null): void {
  selectedInstanceId.value = id;
  currentPage.value = 'launcher';
}

// ── Install state transitions ──

export function enqueueInstall(item: InstallItem): void {
  const active = installState.value;
  if (active.status === 'active' && active.versionId === item.versionId) return;
  if (installQueue.value.some(q => q.versionId === item.versionId)) return;
  installQueue.value = [...installQueue.value, item];
}

export function startInstall(versionId: string, label = 'Starting...'): void {
  installState.value = { status: 'active', versionId, pct: 0, label };
}

export function updateInstallProgress(pct: number, label: string): void {
  const current = installState.value;
  if (current.status !== 'active') return;
  installState.value = { ...current, pct, label };
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

export function setInstallEventSource(es: { close(): void } | null): void {
  if (installEventSource.value) installEventSource.value.close();
  installEventSource.value = es;
}

// ── Launch state transitions ──

export function startLaunch(instanceId: string): void {
  launchState.value = { status: 'preparing', instanceId };
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
