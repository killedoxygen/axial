import { prompt, showChoice } from '../../ui/Dialog';
import type { ContextMenuItem } from '../../ui/ContextMenu';
import { api } from '../../api';
import { toast } from '../../toast';
import type { EnrichedInstance } from '../../types-instance';
import { openInstanceFolder } from './instance-actions';
import { confirmDeleteItems, partialFailureMessage, runBulkMutation, runResourceMutation } from './bulk-actions';
import { dtoString } from '../../dto-contract';
import { requireResourceCommandSuccess } from './resources';

function worldNameError(value: string): string | null {
  return value ? null : 'Use a world name.';
}

export async function renameWorld(inst: EnrichedInstance, worldName: string, onDone: () => void): Promise<void> {
  await runResourceMutation(inst.id, 'Rename world', async () => {
    const next = await prompt('New name for this world', worldName, {
      title: 'Rename world',
      confirmText: 'Rename',
      validate: worldNameError,
    });
    const nextName = next ?? '';
    if (!nextName || nextName === worldName) return;
    const res = await api('PUT', `/instances/${encodeURIComponent(inst.id)}/worlds/${encodeURIComponent(worldName)}`, {
      name: nextName,
    });
    dtoString(requireResourceCommandSuccess(res, 'World rename').name, 'World name');
    toast('World renamed');
    onDone();
  });
}

export async function deleteWorld(inst: EnrichedInstance, worldName: string, onDone: () => void): Promise<void> {
  await runResourceMutation(inst.id, 'Delete world', async () => {
    const choice = await showChoice<'delete'>(
      `Delete "${worldName}" from this instance. This removes the save folder from disk.`,
      [{ value: 'delete', label: 'Delete world', variant: 'danger' }],
      { title: 'Delete world' },
    );
    if (choice !== 'delete') return;
    const res = await api(
      'DELETE',
      `/instances/${encodeURIComponent(inst.id)}/worlds/${encodeURIComponent(worldName)}`,
    );
    requireResourceCommandSuccess(res, 'World deletion');
    toast('World deleted');
    onDone();
  });
}

export async function deleteWorlds(
  inst: EnrichedInstance,
  worldNames: string[],
  onDone: () => void,
  onFailure?: () => void,
): Promise<void> {
  if (worldNames.length === 0) return;
  await runResourceMutation(inst.id, 'Delete worlds', async () => {
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
        const res = await api(
          'DELETE',
          `/instances/${encodeURIComponent(inst.id)}/worlds/${encodeURIComponent(worldName)}`,
        );
        requireResourceCommandSuccess(res, 'World deletion');
      },
      success: (count) => (count === 1 ? 'World deleted' : `${count} worlds deleted`),
      partial: (done, total, err) => partialFailureMessage('Deletion', done, total, err),
      onDone,
      onFailure,
    });
  });
}

export async function backupWorld(inst: EnrichedInstance, worldName: string, onDone: () => void): Promise<void> {
  await runResourceMutation(inst.id, 'Back up world', async () => {
    const res = await api(
      'POST',
      `/instances/${encodeURIComponent(inst.id)}/worlds/${encodeURIComponent(worldName)}/backup`,
      {},
    );
    const result = requireResourceCommandSuccess(res, 'World backup');
    dtoString(result.backup, 'World backup name');
    const location = dtoString(result.location, 'World backup location');
    toast(`World backed up to ${location}`);
    onDone();
  });
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
