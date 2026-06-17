import { installFailure, installQueueState, installState } from './store';
import { isSameInstallItem } from './actions';
import { minecraftVersionLabel } from './version-display';
import type { InstallFailure } from './store';
import type { Version } from './types-version';
import type { InstallItem, InstallQueuedItemViewModel } from './types-install';
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

function matchesInstanceInstall(candidate: InstallItem, expected: InstallItem): boolean {
  if (isSameInstallItem(candidate, expected)) return true;
  return candidate.versionId === expected.versionId;
}

function installItemFromQueuedViewModel(item: InstallQueuedItemViewModel): InstallItem {
  const versionId = item.install_item.version_id;
  if (!item.install_item.loader) return { versionId };
  return {
    versionId,
    loader: {
      componentId: item.install_item.loader.component_id,
      buildId: item.install_item.loader.build_id,
      minecraftVersion: item.install_item.loader.minecraft_version,
      loaderVersion: item.install_item.loader.loader_version,
    },
  };
}

export function instanceInstallStatus(
  inst: InstanceInstallCandidate,
  version: Version | undefined,
): InstanceInstallStatus {
  const expectedItem = installItemForInstance(inst, version);
  const install = installState.value;
  const activeInstall =
    install.status === 'active' && matchesInstanceInstall(install.item, expectedItem) ? install : undefined;
  const activeItem = activeInstall?.item;
  const queuedItem = installQueueState.value.items.find((candidate) =>
    matchesInstanceInstall(installItemFromQueuedViewModel(candidate), expectedItem),
  );
  const failure =
    installFailure.value && matchesInstanceInstall(installFailure.value.item, expectedItem)
      ? installFailure.value
      : null;
  const item =
    activeItem ??
    (queuedItem ? installItemFromQueuedViewModel(queuedItem) : undefined) ??
    failure?.item ??
    expectedItem;
  const progress = activeItem
    ? {
        pct: activeInstall.pct,
        label: activeInstall.label,
        displayName: activeInstall.displayName,
        remainingSeconds: activeInstall.remainingSeconds,
        remainingSecondsUpdatedAt: activeInstall.remainingSecondsUpdatedAt,
      }
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
