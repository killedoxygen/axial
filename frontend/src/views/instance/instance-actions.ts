import { api } from '../../api';
import { toast } from '../../toast';
import { errMessage } from '../../utils';
import { prompt, showChoice } from '../../ui/Dialog';
import { addInstance, updateInstanceInList } from '../../actions';
import type { Instance } from '../../types-instance';
import { partialFailureMessage, runBulkMutation } from './bulk-actions';
import { dtoError } from '../../dto-contract';
import { enrichedInstanceResponse } from '../../dto-core';
import type { DeletionSnapshot } from '../../generated/DeletionSnapshot';
import {
  checkInstanceDeletion,
  continueInstanceDeletion,
  pendingInstanceDeletion,
  refreshPendingInstanceDeletions,
  requestInstanceDeletion,
  type DeletionObservation,
} from './deletions';

function removalResultMessage(result: DeletionSnapshot): string {
  if (result.status === 'aborted') return 'The removal was aborted. The instance and its files were preserved.';
  if (result.status === 'pending_restore')
    return 'The removal was interrupted before completion. Review the restore action in Instances.';
  if (result.status === 'cleanup_pending')
    return 'The instance removal is awaiting file cleanup. Review the cleanup action in Instances.';
  return result.intent === 'keep_files' ? 'Removed from launcher; files kept on disk' : 'Instance deleted';
}

export async function checkInstanceDeletionFlow(observation: DeletionObservation, onDone?: () => void): Promise<void> {
  try {
    const result = await checkInstanceDeletion(observation);
    toast(removalResultMessage(result), result.status === 'removed' ? 'success' : 'info');
    if (result.status === 'removed') onDone?.();
  } catch (error) {
    toast(`Could not check the removal: ${errMessage(error)}`, 'error');
  }
}

export async function recoverInstanceDeletionFlow(
  observation: DeletionObservation,
  onDone?: () => void,
): Promise<void> {
  try {
    const status = await checkInstanceDeletion(observation);
    if (status.status === 'removed' || status.status === 'aborted') {
      toast(removalResultMessage(status), 'info');
      if (status.status === 'removed') onDone?.();
      return;
    }
    const restore = status.status === 'pending_restore';
    const choice = await showChoice<'continue'>(
      restore
        ? 'The earlier removal did not commit. Restore the instance and its files using that same operation. This does not start another deletion.'
        : status.intent === 'keep_files'
          ? 'Finish the earlier launcher removal. Its original choice to keep files on disk will be preserved.'
          : 'The earlier removal already committed. Finish deleting its remaining files using that same operation.',
      [
        {
          value: 'continue',
          label: restore ? 'Restore instance' : 'Finish cleanup',
          variant: restore ? 'secondary' : 'danger',
        },
      ],
      { title: restore ? 'Restore interrupted removal' : 'Finish interrupted removal' },
    );
    if (!choice) return;
    const result = await continueInstanceDeletion(status);
    toast(removalResultMessage(result), result.status === 'removed' ? 'success' : 'info');
    if (result.status === 'removed') onDone?.();
  } catch (error) {
    toast(`Could not recover the removal: ${errMessage(error)}`, 'error');
  }
}

export async function openInstanceFolder(id: string, sub?: string): Promise<void> {
  try {
    const suffix = sub ? `?sub=${encodeURIComponent(sub)}` : '';
    const error = dtoError(await api('POST', `/instances/${encodeURIComponent(id)}/open-folder${suffix}`));
    if (error) toast(`Could not open the instance folder: ${error}`, 'error');
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
    const res = enrichedInstanceResponse(await api('PUT', `/instances/${encodeURIComponent(inst.id)}`, { name: next }));
    updateInstanceInList(res);
    toast('Renamed');
  } catch (err) {
    toast(`Could not rename the instance: ${errMessage(err)}`, 'error');
  }
}

export async function duplicateInstance(inst: Instance): Promise<void> {
  try {
    const res = enrichedInstanceResponse(await api('POST', `/instances/${encodeURIComponent(inst.id)}/duplicate`, {}));
    addInstance(res);
    toast('Duplicated');
  } catch (err) {
    toast(`Could not duplicate the instance: ${errMessage(err)}`, 'error');
  }
}

export async function deleteInstanceFlow(inst: Instance, onDone?: () => void): Promise<void> {
  try {
    await refreshPendingInstanceDeletions();
    const pending = pendingInstanceDeletion(inst.id);
    if (pending) {
      if (pending.status === 'unknown') await checkInstanceDeletionFlow(pending, onDone);
      else await recoverInstanceDeletionFlow(pending, onDone);
      return;
    }
  } catch (error) {
    toast(`Could not check unfinished removals: ${errMessage(error)}`, 'error');
    return;
  }
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
    const result = await requestInstanceDeletion(inst.id, keepFiles ? 'keep_files' : 'delete_files');
    toast(removalResultMessage(result), result.status === 'removed' ? 'success' : 'info');
    if (result.status === 'removed') onDone?.();
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
  try {
    await runBulkMutation({
      items: selected,
      action: async (inst) => {
        const result = await requestInstanceDeletion(inst.id, keepFiles ? 'keep_files' : 'delete_files');
        if (result.status !== 'removed') throw new Error(removalResultMessage(result));
      },
      success: (count) => (keepFiles ? `${count} instances removed; files kept on disk` : `${count} instances deleted`),
      partial: (done, total, err) => partialFailureMessage('Removal', done, total, err),
      onDone: () => onDone?.(),
    });
  } catch (error) {
    toast(errMessage(error), 'error');
  }
}
