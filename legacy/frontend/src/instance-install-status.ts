import { activeDownload, downloadFailure, downloadQueue, type DownloadFailure } from './machines/downloads';
import { installItemFromQueuedViewModel, isSameInstallItem } from './install-item';
import { minecraftVersionLabel } from './version-display';
import type { Version } from './types-version';
import type { InstallItem, InstallQueuedItemViewModel } from './types-install';
import type { EnrichedInstance } from './types-instance';

export type InstanceInstallCandidate = Pick<EnrichedInstance, 'version_id'> &
  Partial<Pick<EnrichedInstance, 'needs_install'>>;

export type InstanceInstallProgress = {
  pct: number;
  label: string;
  displayName?: string;
};

export type InstanceInstallStatus = {
  item: InstallItem;
  target: string;
  state: 'idle' | 'active' | 'queued' | 'failed';
  installing: boolean;
  label: string;
  progress: InstanceInstallProgress | null;
  queuedItem?: InstallQueuedItemViewModel;
  failure: DownloadFailure | null;
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

export function instanceInstallStatus(
  inst: InstanceInstallCandidate,
  version: Version | undefined,
): InstanceInstallStatus {
  const expectedItem = installItemForInstance(inst, version);
  const allowVersionOnlyMatch = version === undefined;
  const active = activeDownload.value;
  const activeInstall =
    active && matchesInstanceInstall(active.item, expectedItem, allowVersionOnlyMatch) ? active : null;
  const queuedItem = downloadQueue.value.items.find((candidate) =>
    matchesInstanceInstall(installItemFromQueuedViewModel(candidate), expectedItem, allowVersionOnlyMatch),
  );
  const failure =
    downloadFailure.value && matchesInstanceInstall(downloadFailure.value.item, expectedItem, allowVersionOnlyMatch)
      ? downloadFailure.value
      : null;
  const item =
    activeInstall?.item ??
    (queuedItem ? installItemFromQueuedViewModel(queuedItem) : undefined) ??
    failure?.item ??
    expectedItem;
  const progress = activeInstall
    ? { pct: activeInstall.pct, label: activeInstall.label, displayName: activeInstall.displayName }
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
