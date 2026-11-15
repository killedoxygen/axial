import { activeDownload, downloadFailures, downloadQueue, installedStateSettling, type DownloadFailure } from './queue';
import { isSameInstallItem } from './item';
import type { InstallItem } from '../generated/InstallItem';
import type { InstallQueuedItemViewModel } from '../generated/InstallQueuedItemViewModel';
import type { EnrichedInstance } from '../generated/EnrichedInstance';

export type InstanceInstallCandidate = Pick<EnrichedInstance, 'version_id' | 'install_target'>;

export type InstanceInstallProgress = {
  pct: number;
  label: string;
  displayName?: string;
};

export type InstanceInstallStatus = {
  item: InstallItem | null;
  target: string;
  state: 'idle' | 'active' | 'queued' | 'failed';
  installing: boolean;
  /** Nothing is installing any more, but the instance has not been re-read since: its readiness is from before. */
  settling: boolean;
  label: string;
  progress: InstanceInstallProgress | null;
  queuedItem?: InstallQueuedItemViewModel;
  failure: DownloadFailure | null;
};

/** The install this instance waits on, wherever it is: running, queued, failed or not started. */
export function instanceInstallStatus(inst: InstanceInstallCandidate): InstanceInstallStatus {
  const expectedItem = inst.install_target;
  const matches = (candidate: InstallItem): boolean => !!expectedItem && isSameInstallItem(candidate, expectedItem);
  const active = activeDownload.value;
  const activeInstall = active && matches(active.item) ? active : null;
  const queuedItem = downloadQueue.value.items.find((candidate) => matches(candidate.install_item));
  const failure = downloadFailures.value.find((candidate) => matches(candidate.item)) ?? null;
  const item =
    activeInstall?.item ?? (queuedItem ? queuedItem.install_item : undefined) ?? failure?.item ?? expectedItem;
  const progress = activeInstall
    ? { pct: activeInstall.ringPct, label: activeInstall.label, displayName: activeInstall.displayName }
    : null;
  const state = progress ? 'active' : queuedItem ? 'queued' : failure ? 'failed' : 'idle';
  const label =
    progress?.displayName || queuedItem?.label || failure?.displayName || item?.version_id || inst.version_id;

  return {
    item,
    target: item?.version_id ?? '',
    state,
    installing: state === 'active' || state === 'queued',
    settling: state === 'idle' && installedStateSettling.value,
    label,
    progress,
    queuedItem,
    failure,
  };
}
