import { prompt, showChoice } from '../../ui/Dialog';
import type { ContextMenuItem } from '../../ui/ContextMenu';
import { api } from '../../api';
import { toast } from '../../toast';
import { errMessage } from '../../utils';
import type { EnrichedInstance } from '../../types-instance';
import { openInstanceFolder } from './instance-actions';
import { confirmDeleteItems, partialFailureMessage, runBulkMutation } from './bulk-actions';

function worldNameError(value: string): string | null {
  return value ? null : 'Use a world name.';
}

export async function renameWorld(inst: EnrichedInstance, worldName: string, onDone: () => void): Promise<void> {
  const next = await prompt('New name for this world', worldName, {
    title: 'Rename world',
    confirmText: 'Rename',
    validate: worldNameError,
  });
  const nextName = next ?? '';
  if (!nextName || nextName === worldName) return;
  try {
    const res: any = await api(
      'PUT',
      `/instances/${encodeURIComponent(inst.id)}/worlds/${encodeURIComponent(worldName)}`,
      { name: nextName },
    );
    if (res?.error) throw new Error(res.error);
    toast('World renamed');
    onDone();
  } catch (err) {
    toast(`Could not rename the world: ${errMessage(err)}`, 'error');
  }
}

export async function deleteWorld(inst: EnrichedInstance, worldName: string, onDone: () => void): Promise<void> {
  const choice = await showChoice<'delete'>(
    `Delete "${worldName}" from this instance. This removes the save folder from disk.`,
    [{ value: 'delete', label: 'Delete world', variant: 'danger' }],
    { title: 'Delete world' },
  );
  if (choice !== 'delete') return;
  try {
    const res: any = await api(
      'DELETE',
      `/instances/${encodeURIComponent(inst.id)}/worlds/${encodeURIComponent(worldName)}`,
    );
    if (res?.error) throw new Error(res.error);
    toast('World deleted');
    onDone();
  } catch (err) {
    toast(`Could not delete the world: ${errMessage(err)}`, 'error');
  }
}

export async function deleteWorlds(inst: EnrichedInstance, worldNames: string[], onDone: () => void): Promise<void> {
  const confirmed = await confirmDeleteItems({
    count: worldNames.length,
    itemLabel: 'world',
    message:
      worldNames.length === 1
        ? `Delete "${worldNames[0]!}" from this instance. This removes the save folder from disk.`
        : `Delete ${worldNames.length} worlds from this instance. This removes the selected save folders from disk.`,
  });
  if (!confirmed) return;
  await runBulkMutation({
    items: worldNames,
    action: async (worldName) => {
      const res: any = await api(
        'DELETE',
        `/instances/${encodeURIComponent(inst.id)}/worlds/${encodeURIComponent(worldName)}`,
      );
      if (res?.error) throw new Error(res.error);
    },
    success: (count) => (count === 1 ? 'World deleted' : `${count} worlds deleted`),
    partial: (done, total, err) => partialFailureMessage('Deleted', done, total, err),
    onDone,
  });
}

export async function backupWorld(inst: EnrichedInstance, worldName: string, onDone: () => void): Promise<void> {
  try {
    const res: any = await api(
      'POST',
      `/instances/${encodeURIComponent(inst.id)}/worlds/${encodeURIComponent(worldName)}/backup`,
      {},
    );
    if (res?.error) throw new Error(res.error);
    toast(res?.location ? `World backed up to ${res.location}` : 'World backed up');
    onDone();
  } catch (err) {
    toast(`Could not back up the world: ${errMessage(err)}`, 'error');
  }
}

export function worldMenuItems(inst: EnrichedInstance, worldName: string, onDone: () => void): ContextMenuItem[] {
  return [
    { icon: 'edit', label: 'Rename', onSelect: () => void renameWorld(inst, worldName, onDone) },
    { icon: 'archive', label: 'Back up', onSelect: () => void backupWorld(inst, worldName, onDone) },
    { icon: 'folder', label: 'Open saves folder', onSelect: () => void openInstanceFolder(inst.id, 'saves') },
    { divider: true, label: '', onSelect: () => undefined },
    { icon: 'trash', label: 'Delete', onSelect: () => void deleteWorld(inst, worldName, onDone), danger: true },
  ];
}
