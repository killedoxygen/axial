import { prompt, showChoice } from '../../ui/Dialog';
import type { ContextMenuItem } from '../../ui/ContextMenu';
import { api } from '../../api';
import { toast } from '../../toast';
import { errMessage } from '../../utils';
import type { EnrichedInstance } from '../../types';
import { openInstanceFolder } from './instance-actions';

function worldNameError(value: string): string | null {
  const name = value.trim();
  if (!name || name === '.' || name === '..') return 'Use a world name.';
  if (name.startsWith('.')) return 'World names cannot start with a dot.';
  if (/[\\/]/.test(name)) return 'World names cannot include folders.';
  if (/[\u0000-\u001f\u007f]/.test(name)) return 'World names cannot include control characters.';
  return null;
}

export async function renameWorld(inst: EnrichedInstance, worldName: string, onDone: () => void): Promise<void> {
  const next = await prompt('New name for this world', worldName, {
    title: 'Rename world',
    confirmText: 'Rename',
    validate: worldNameError,
  });
  const nextName = next?.trim() ?? '';
  if (!nextName || nextName === worldName) return;
  try {
    const res: any = await api('PUT', `/instances/${encodeURIComponent(inst.id)}/worlds/${encodeURIComponent(worldName)}`, { name: nextName });
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
    const res: any = await api('DELETE', `/instances/${encodeURIComponent(inst.id)}/worlds/${encodeURIComponent(worldName)}`);
    if (res?.error) throw new Error(res.error);
    toast('World deleted');
    onDone();
  } catch (err) {
    toast(`Could not delete the world: ${errMessage(err)}`, 'error');
  }
}

export async function backupWorld(inst: EnrichedInstance, worldName: string, onDone: () => void): Promise<void> {
  try {
    const res: any = await api('POST', `/instances/${encodeURIComponent(inst.id)}/worlds/${encodeURIComponent(worldName)}/backup`, {});
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
