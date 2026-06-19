import { installFailure, installQueueState, installState } from './store';
import { isSameInstallItem } from './actions';
import { minecraftVersionLabel } from './version-display';
import type { InstallFailure } from './store';
import type { Version } from './types-version';
import type {
  InstallItem,
  InstallQueueActiveViewModel,
  InstallQueueInstallItemViewModel,
  InstallQueuedItemViewModel,
} from './types-install';
import type { EnrichedInstance } from './types-instance';

export type InstanceInstallCandidate = Pick<EnrichedInstance, 'version_id'> &
  Partial<Pick<EnrichedInstance, 'needs_install'>>;

export type InstanceInstallProgress = {
  pct: number;
  label: string;
  displayName?: string;
  remainingSeconds?: number;
  remainingSecondsUpdatedAt?: number;
};

export type InstanceInstallStatus = {
  item: InstallItem;
  target: string;
  state: 'idle' | 'active' | 'queued' | 'failed';
  installing: boolean;
  label: string;
  progress: InstanceInstallProgress | null;
  queuedItem?: InstallQueuedItemViewModel;
  failure: InstallFailure | null;
};

export function installTargetForInstance(inst: InstanceInstallCandidate, version: Version | undefined): string {
  return version?.needs_install || inst.needs_install || version?.id || inst.version_id;
}

export function installItemForInstance(inst: InstanceInstallCandidate, version: Version | undefined): InstallItem {
  const versionId = installTargetForInstance(inst, version);
  if (!version?.loader) return { versionId };
  return {
    versionId,
    loader: {
      componentId: version.loader.component_id,
      buildId: version.loader.build_id,
      minecraftVersion: minecraftVersionLabel(version, ''),
      loaderVersion: version.loader.loader_version,
    },
  };
}

function matchesInstanceInstall(candidate: InstallItem, expected: InstallItem, allowVersionOnly: boolean): boolean {
  if (allowVersionOnly) return candidate.versionId === expected.versionId;
  return isSameInstallItem(candidate, expected);
}

export function installItemFromQueueInstallItem(value: InstallQueueInstallItemViewModel): InstallItem {
  const versionId = value.version_id;
  if (!value.loader) return { versionId };
  return {
    versionId,
    loader: {
      componentId: value.loader.component_id,
      buildId: value.loader.build_id,
      minecraftVersion: value.loader.minecraft_version,
      loaderVersion: value.loader.loader_version,
    },
  };
}

export function installItemFromQueuedViewModel(item: InstallQueuedItemViewModel): InstallItem {
  return installItemFromQueueInstallItem(item.install_item);
}

function progressFromLocalActive(
  active: Extract<typeof installState.value, { status: 'active' }>,
): InstanceInstallProgress {
  return {
    pct: active.pct,
    label: active.label,
    displayName: active.displayName,
    remainingSeconds: active.remainingSeconds,
    remainingSecondsUpdatedAt: active.remainingSecondsUpdatedAt,
  };
}

function progressFromQueueActive(active: InstallQueueActiveViewModel): InstanceInstallProgress {
  return {
    pct: active.progress.progress_pct,
    label: active.progress.label,
    displayName: active.label,
  };
}

export function instanceInstallStatus(
  inst: InstanceInstallCandidate,
  version: Version | undefined,
): InstanceInstallStatus {
  const expectedItem = installItemForInstance(inst, version);
  const allowVersionOnlyMatch = version === undefined;
  const install = installState.value;
  const queueActive = installQueueState.value.active ?? null;
  const queueActiveItem = queueActive ? installItemFromQueueInstallItem(queueActive.install_item) : null;
  const activeQueueInstall =
    queueActive && queueActiveItem && matchesInstanceInstall(queueActiveItem, expectedItem, allowVersionOnlyMatch)
      ? { active: queueActive, item: queueActiveItem }
      : null;
  const localActiveInstall =
    install.status === 'active' && matchesInstanceInstall(install.item, expectedItem, allowVersionOnlyMatch)
      ? install
      : undefined;
  const activeItem = activeQueueInstall?.item ?? localActiveInstall?.item;
  const queuedItem = installQueueState.value.items.find((candidate) =>
    matchesInstanceInstall(installItemFromQueuedViewModel(candidate), expectedItem, allowVersionOnlyMatch),
  );
  const failure =
    installFailure.value && matchesInstanceInstall(installFailure.value.item, expectedItem, allowVersionOnlyMatch)
      ? installFailure.value
      : null;
  const item =
    activeItem ??
    (queuedItem ? installItemFromQueuedViewModel(queuedItem) : undefined) ??
    failure?.item ??
    expectedItem;
  const progress = activeQueueInstall
    ? localActiveInstall &&
      matchesInstanceInstall(localActiveInstall.item, activeQueueInstall.item, allowVersionOnlyMatch)
      ? progressFromLocalActive(localActiveInstall)
      : progressFromQueueActive(activeQueueInstall.active)
    : localActiveInstall
      ? progressFromLocalActive(localActiveInstall)
      : null;
  const state = progress ? 'active' : queuedItem ? 'queued' : failure ? 'failed' : 'idle';
  const label = progress?.displayName || queuedItem?.label || failure?.displayName || item.versionId;

  return {
    item,
    target: item.versionId,
    state,
    installing: state === 'active' || state === 'queued',
    label,
    progress,
    queuedItem,
    failure,
  };
}
