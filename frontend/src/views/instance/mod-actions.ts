import { api } from '../../api';
import { toast } from '../../toast';
import { errMessage } from '../../utils';
import type { ContextMenuItem } from '../../ui/ContextMenu';
import type { EnrichedInstance, InstanceMod } from '../../types';
import { openInstanceFolder } from './instance-actions';
import { confirmDeleteItems, partialFailureMessage, runBulkMutation } from './bulk-actions';

async function updateModEnabled(inst: EnrichedInstance, modName: string, enabled: boolean): Promise<void> {
  const res: any = await api('PUT', `/instances/${encodeURIComponent(inst.id)}/mods/${encodeURIComponent(modName)}`, {
    enabled,
  });
  if (res?.error) throw new Error(res.error);
}

async function removeMod(inst: EnrichedInstance, modName: string): Promise<void> {
  const res: any = await api('DELETE', `/instances/${encodeURIComponent(inst.id)}/mods/${encodeURIComponent(modName)}`);
  if (res?.error) throw new Error(res.error);
}

export async function setModEnabled(inst: EnrichedInstance, mod: InstanceMod, onDone: () => void): Promise<void> {
  try {
    await updateModEnabled(inst, mod.name, !mod.enabled);
    toast(!mod.enabled ? 'Mod enabled' : 'Mod disabled');
    onDone();
  } catch (err) {
    toast(`Could not update the mod: ${errMessage(err)}`, 'error');
  }
}

export async function setModsEnabled(
  inst: EnrichedInstance,
  mods: InstanceMod[],
  enabled: boolean,
  onDone: () => void,
): Promise<void> {
  const changed = mods.filter((mod) => mod.enabled !== enabled);
  if (changed.length === 0) {
    toast(enabled ? 'Selected mods are already enabled' : 'Selected mods are already disabled', 'info');
    return;
  }
  await runBulkMutation({
    items: changed,
    action: (mod) => updateModEnabled(inst, mod.name, enabled),
    success: (count) => (enabled ? `${count} mods enabled` : `${count} mods disabled`),
    partial: (done, total, err) => partialFailureMessage('Updated', done, total, err),
    onDone,
  });
}

export async function deleteMods(inst: EnrichedInstance, mods: InstanceMod[], onDone: () => void): Promise<void> {
  const confirmed = await confirmDeleteItems({
    count: mods.length,
    itemLabel: 'mod',
    message:
      mods.length === 1
        ? `Delete "${mods[0]!.name}" from this instance. This removes the mod file from disk.`
        : `Delete ${mods.length} mods from this instance. This removes the selected mod files from disk.`,
  });
  if (!confirmed) return;
  await runBulkMutation({
    items: mods,
    action: (mod) => removeMod(inst, mod.name),
    success: (count) => (count === 1 ? 'Mod deleted' : `${count} mods deleted`),
    partial: (done, total, err) => partialFailureMessage('Deleted', done, total, err),
    onDone,
  });
}

export function modMenuItems(
  inst: EnrichedInstance,
  mod: InstanceMod,
  onRefresh: () => void,
  selectionItem: ContextMenuItem,
): ContextMenuItem[] {
  return [
    selectionItem,
    { divider: true, label: '', onSelect: () => undefined },
    {
      icon: mod.enabled ? 'stop' : 'play',
      label: mod.enabled ? 'Disable' : 'Enable',
      onSelect: () => void setModEnabled(inst, mod, onRefresh),
    },
    { icon: 'folder', label: 'Open mods folder', onSelect: () => void openInstanceFolder(inst.id, 'mods') },
    { icon: 'refresh', label: 'Refresh list', onSelect: onRefresh },
    { divider: true, label: '', onSelect: () => undefined },
    { icon: 'trash', label: 'Delete', onSelect: () => void deleteMods(inst, [mod], onRefresh), danger: true },
  ];
}
