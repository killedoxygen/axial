import { batch } from '@preact/signals';
import {
  instances,
  versions,
  config,
  systemInfo,
  devMode,
  catalog,
  selectedInstanceId,
  lastInstanceId,
  launchState,
  runningSessions,
  launchNotices,
  currentPage,
  searchQuery,
  sidebarFilter,
  logLines,
} from './store';
import type { RunningSession, LaunchNotice } from './types-launch';
import type { Version, Catalog } from './types-version';
import type { Instance } from './types-instance';
import type { Config, SystemInfo } from './types-settings';
import type { Page } from './types-ui';
import type { LaunchStatusViewModel } from './types-launch';

export function selectInstance(id: string | null): void {
  selectedInstanceId.value = id;
  currentPage.value = 'launcher';
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

export function updateRunningSessionState(instanceId: string, patch: Partial<RunningSession>): void {
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

export function setVersions(v: Version[]): void {
  versions.value = v;
}
export function setInstances(i: Instance[]): void {
  instances.value = i;
}
export function setConfig(c: Config): void {
  config.value = c;
}
export function setSystemInfo(s: SystemInfo): void {
  systemInfo.value = s;
}
export function setDevMode(d: boolean): void {
  devMode.value = d;
}
export function setCatalog(c: Catalog | null): void {
  catalog.value = c;
}
export function setLastInstanceId(id: string | null): void {
  lastInstanceId.value = id;
}

export function navigate(page: Page): void {
  currentPage.value = page;
}
export function setSearch(q: string): void {
  searchQuery.value = q;
}
export function setFilter(f: string): void {
  sidebarFilter.value = f;
}
export function setLogLines(n: number): void {
  logLines.value = n;
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
