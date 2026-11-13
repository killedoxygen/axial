import { api } from '../../api';
import { uninstallContents } from '../../content';
import { applyInstallQueueResponse } from '../../machines/downloads';
import { toast } from '../../toast';
import { navigate } from '../../ui-state';
import type { ContextMenuItem } from '../../ui/ContextMenu';
import type { ContentUpdate, InstanceContentEntry } from '../../types-content';
import type { EnrichedInstance } from '../../types-instance';
import type { InstancePackInfo } from '../../generated/InstancePackInfo';
import { openInstanceFolder } from './instance-actions';
import { confirmDeleteItems, partialFailureMessage, runResourceMutation } from './bulk-actions';
import { applyModUpdates, updateMenuLabel } from './mod-actions';
import { requireResourceCommandSuccess, type PackFolder } from './resources';

export interface PackRow {
  pack: InstancePackInfo;
  entry?: InstanceContentEntry;
  update?: ContentUpdate;
}

export const PACK_NOUN: Record<PackFolder, string> = {
  resourcepacks: 'resource pack',
  shaderpacks: 'shader pack',
};

/** Managed packs leave through the content queue; local archives are deleted directly. Folders stay. */
export async function removePacks(
  inst: EnrichedInstance,
  folder: PackFolder,
  rows: PackRow[],
  onDone: () => void,
  onFailure?: () => void,
): Promise<void> {
  const noun = PACK_NOUN[folder];
  const removable = rows.filter((row) => row.entry || !row.pack.directory);
  if (removable.length < rows.length) {
    toast('Unpacked folders are kept. Remove them from the folder itself.', 'info');
  }
  if (removable.length === 0) return;
  await runResourceMutation(inst.id, `Remove ${noun}s`, async () => {
    const confirmed = await confirmDeleteItems({
      count: removable.length,
      itemLabel: noun,
      message:
        removable.length === 1
          ? `Remove "${removable[0]!.entry?.title ?? removable[0]!.pack.name}" from this instance.`
          : `Remove ${removable.length} ${noun}s from this instance.`,
    });
    if (!confirmed) return;
    const managed = removable.flatMap((row) => (row.entry ? [row.entry] : []));
    const local = removable.filter((row) => !row.entry);
    let started = 0;
    try {
      if (managed.length > 0) {
        const queue = await uninstallContents(
          inst.id,
          managed.map((entry) => entry.canonical_id),
        );
        await applyInstallQueueResponse(queue, { showNotice: false, connectActive: true });
        started += managed.length;
      }
      for (const row of local) {
        requireResourceCommandSuccess(
          await api(
            'DELETE',
            `/instances/${encodeURIComponent(inst.id)}/${folder}/${encodeURIComponent(row.pack.name)}`,
          ),
          `${noun} deletion`,
        );
        started += 1;
      }
      toast(started === 1 ? `Removing ${noun}` : `Removing ${started} ${noun}s`);
    } catch (err) {
      onFailure?.();
      throw new Error(partialFailureMessage('Removal request', started, removable.length, err));
    }
    onDone();
  });
}

export function packMenuItems(
  inst: EnrichedInstance,
  folder: PackFolder,
  row: PackRow,
  onRefresh: () => void,
  selectionItem: ContextMenuItem,
): ContextMenuItem[] {
  const items: ContextMenuItem[] = [selectionItem, { divider: true, label: '', onSelect: () => undefined }];
  const { entry, update } = row;
  if (update) {
    items.push({
      icon: 'arrow-up',
      label: updateMenuLabel(inst, update),
      onSelect: () => void applyModUpdates(inst, [update]),
    });
  }
  if (entry) {
    items.push({
      icon: 'compass',
      label: 'View in Discover',
      onSelect: () => navigate({ name: 'content', id: entry.canonical_id, target: inst.id }),
    });
  }
  items.push(
    { icon: 'folder', label: 'Open folder', onSelect: () => void openInstanceFolder(inst.id, folder) },
    { icon: 'refresh', label: 'Refresh list', onSelect: onRefresh },
  );
  if (entry || !row.pack.directory) {
    items.push(
      { divider: true, label: '', onSelect: () => undefined },
      {
        icon: 'trash',
        label: 'Remove',
        danger: true,
        onSelect: () => void removePacks(inst, folder, [row], onRefresh, onRefresh),
      },
    );
  }
  return items;
}
