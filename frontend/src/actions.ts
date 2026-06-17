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
  installState,
  installQueueState,
  installFailure,
  installEventSource,
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
import type { InstallFailureViewModel, InstallItem, InstallQueueStateResponse } from './types-install';
import type { Instance } from './types-instance';
import type { Config, SystemInfo } from './types-settings';
import type { Page } from './types-ui';
import type { InstallStepProgress } from './store';
import type { LaunchStatusViewModel } from './types-launch';

export function selectInstance(id: string | null): void {
  selectedInstanceId.value = id;
  currentPage.value = 'launcher';
}

function cloneInstallItem(item: InstallItem): InstallItem {
  return item.loader ? { versionId: item.versionId, loader: { ...item.loader } } : { versionId: item.versionId };
}

let activeInstallItem: InstallItem | null = null;

export function isSameInstallItem(left: InstallItem, right: InstallItem): boolean {
  if (left.versionId !== right.versionId) return false;
  if (!left.loader && !right.loader) return true;
  if (!left.loader || !right.loader) return false;
  return left.loader.componentId === right.loader.componentId && left.loader.buildId === right.loader.buildId;
}

export function isActiveInstallItem(item: InstallItem): boolean {
  return (
    installState.value.status === 'active' && activeInstallItem !== null && isSameInstallItem(activeInstallItem, item)
  );
}

export function setInstallQueueState(state: InstallQueueStateResponse): void {
  installQueueState.value = state;
}

export function recordInstallFailure(item: InstallItem, displayName: string, viewModel: InstallFailureViewModel): void {
  installFailure.value = {
    item: cloneInstallItem(item),
    displayName,
    viewModel,
    failedAt: Date.now(),
  };
}

export function clearInstallFailure(): void {
  installFailure.value = null;
}

export function clearInstallFailureForItem(item: InstallItem): void {
  const failure = installFailure.value;
  if (!failure || !isSameInstallItem(failure.item, item)) return;
  installFailure.value = null;
}

export function startInstall(item: InstallItem, label = 'Starting...', displayName?: string): void {
  const installItem = cloneInstallItem(item);
  activeInstallItem = installItem;
  installState.value = {
    status: 'active',
    item: installItem,
    versionId: item.versionId,
    displayName,
    pct: 0,
    label,
    phase: 'starting',
    startedAt: Date.now(),
  };
}

function cleanRemainingSeconds(remainingSeconds: number | undefined): number | undefined {
  return typeof remainingSeconds === 'number' && Number.isFinite(remainingSeconds) && remainingSeconds > 0
    ? remainingSeconds
    : undefined;
}

export function updateInstallProgress(
  pct: number,
  label: string,
  phase?: string,
  remainingSeconds?: number,
  activeStep?: InstallStepProgress,
): void {
  const current = installState.value;
  if (current.status !== 'active') return;
  const nextPct = Number.isFinite(pct) ? Math.max(0, Math.min(100, pct)) : current.pct;
  const regressed = nextPct < current.pct;
  const nextRemainingSeconds = cleanRemainingSeconds(remainingSeconds);
  const remainingSecondsUpdatedAt = nextRemainingSeconds ? Date.now() : undefined;
  installState.value = {
    ...current,
    pct: Math.max(current.pct, nextPct),
    label: regressed ? current.label : label,
    phase: regressed ? current.phase : phase || current.phase,
    activeStep: regressed ? current.activeStep : activeStep,
    remainingSeconds: regressed ? current.remainingSeconds : nextRemainingSeconds,
    remainingSecondsUpdatedAt: regressed ? current.remainingSecondsUpdatedAt : remainingSecondsUpdatedAt,
  };
}

export function completeInstall(): void {
  activeInstallItem = null;
  installState.value = { status: 'idle' };
  if (installEventSource.value) {
    installEventSource.value.close();
    installEventSource.value = null;
  }
}

export function setInstallEventSource(es: { close(): void } | null): void {
  if (installEventSource.value) installEventSource.value.close();
  installEventSource.value = es;
}

export function startLaunch(instanceId: string): void {
  launchState.value = { status: 'preparing', instanceId, pct: 0, label: 'Starting launch' };
}

export function updateLaunchPrep(instanceId: string, pct: number, label: string, stage?: string): void {
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
