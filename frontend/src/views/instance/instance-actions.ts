import { api } from '../../api';
import { toast } from '../../toast';
import { errMessage } from '../../utils';
import { prompt, showChoice } from '../../ui/Dialog';
import { addInstance, removeInstance, updateInstanceInList } from '../../actions';
import type { Instance } from '../../types';
import { partialFailureMessage, runBulkMutation } from './bulk-actions';

// Instance-level actions shared between InstanceDetailView and InstancesView,
// plus the folder helper used across this view's cards and panes.

export async function openInstanceFolder(id: string, sub?: string): Promise<void> {
  try {
    const suffix = sub ? `?sub=${encodeURIComponent(sub)}` : '';
    const res: any = await api('POST', `/instances/${encodeURIComponent(id)}/open-folder${suffix}`);
    if (res?.error) toast(`Could not open the instance folder: ${res.error}`, 'error');
  } catch (err) {
    toast(`Could not open the instance folder: ${errMessage(err)}`, 'error');
  }
}

export async function renameInstance(inst: Instance): Promise<void> {
  const next = await prompt('New name for this instance', inst.name, {
    title: 'Rename instance',
    confirmText: 'Rename',
  });
  if (!next || next === inst.name) return;
  try {
    const res: any = await api('PUT', `/instances/${encodeURIComponent(inst.id)}`, { name: next });
    if (res.error) throw new Error(res.error);
    updateInstanceInList(res);
    toast('Renamed');
  } catch (err) {
    toast(`Could not rename the instance: ${errMessage(err)}`, 'error');
  }
}

export async function duplicateInstance(inst: Instance): Promise<void> {
  try {
    const res: any = await api('POST', `/instances/${encodeURIComponent(inst.id)}/duplicate`, {});
    if (res.error) throw new Error(res.error);
    addInstance(res);
    toast('Duplicated');
  } catch (err) {
    toast(`Could not duplicate the instance: ${errMessage(err)}`, 'error');
  }
}

export async function deleteInstanceFlow(inst: Instance, onDone?: () => void): Promise<void> {
  const choice = await showChoice<'keep-files' | 'delete-files'>(
    `Remove "${inst.name}" from the launcher but keep files on disk, or delete the instance and its saves, mods, and config.`,
    [
      { value: 'keep-files', label: 'Remove, keep files', variant: 'secondary' },
      { value: 'delete-files', label: 'Delete instance and files', variant: 'danger' },
    ],
    { title: 'Remove instance' },
  );
  if (!choice) return;
  const keepFiles = choice === 'keep-files';
  try {
    const suffix = keepFiles ? '?keep_files=true' : '';
    const res: any = await api('DELETE', `/instances/${encodeURIComponent(inst.id)}${suffix}`);
    if (res?.error) throw new Error(res.error);
    removeInstance(inst.id);
    toast(keepFiles ? 'Removed from launcher; files kept on disk' : 'Instance deleted');
    onDone?.();
  } catch (err) {
    toast(`Could not remove the instance: ${errMessage(err)}`, 'error');
  }
}

export async function deleteInstancesFlow(selected: Instance[], onDone?: () => void): Promise<void> {
  if (selected.length === 0) return;
  if (selected.length === 1) {
    await deleteInstanceFlow(selected[0]!, onDone);
    return;
  }
  const choice = await showChoice<'keep-files' | 'delete-files'>(
    `Remove ${selected.length} instances from the launcher. You can keep files on disk, or delete the selected instances and their saves, mods, and config.`,
    [
      { value: 'keep-files', label: 'Remove, keep files', variant: 'secondary' },
      { value: 'delete-files', label: 'Delete instances and files', variant: 'danger' },
    ],
    { title: 'Remove selected instances' },
  );
  if (!choice) return;
  const keepFiles = choice === 'keep-files';
  await runBulkMutation({
    items: selected,
    action: async (inst) => {
      const suffix = keepFiles ? '?keep_files=true' : '';
      const res: any = await api('DELETE', `/instances/${encodeURIComponent(inst.id)}${suffix}`);
      if (res?.error) throw new Error(res.error);
      removeInstance(inst.id);
    },
    success: (count) => (keepFiles ? `${count} instances removed; files kept on disk` : `${count} instances deleted`),
    partial: (done, total, err) => partialFailureMessage('Removed', done, total, err),
    onDone: () => onDone?.(),
  });
}
