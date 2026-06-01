import type { JSX } from 'preact';
import { Icon } from '../../../ui/Icons';
import { openContextMenu } from '../../../ui/ContextMenu';
import { prompt, showChoice } from '../../../ui/Dialog';
import { api } from '../../../api';
import { toast } from '../../../toast';
import { errMessage } from '../../../utils';
import type { EnrichedInstance } from '../../../types';
import { fmtBytes, fmtRelative } from '../format';
import type { ResourceLoadState } from '../resources';
import { openInstanceFolder } from '../instance-actions';
import { ResourceEmpty, ResourceRow, ResourceStatus, ResourceToolbar } from '../components/resource-bits';

function worldNameError(value: string): string | null {
  const name = value.trim();
  if (!name || name === '.' || name === '..') return 'Use a world name.';
  if (name.startsWith('.')) return 'World names cannot start with a dot.';
  if (/[\\/]/.test(name)) return 'World names cannot include folders.';
  if (/[\u0000-\u001f\u007f]/.test(name)) return 'World names cannot include control characters.';
  return null;
}

async function renameWorld(inst: EnrichedInstance, worldName: string, onDone: () => void): Promise<void> {
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

async function deleteWorld(inst: EnrichedInstance, worldName: string, onDone: () => void): Promise<void> {
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

async function backupWorld(inst: EnrichedInstance, worldName: string, onDone: () => void): Promise<void> {
  try {
    const res: any = await api('POST', `/instances/${encodeURIComponent(inst.id)}/worlds/${encodeURIComponent(worldName)}/backup`, {});
    if (res?.error) throw new Error(res.error);
    toast(res?.location ? `World backed up to ${res.location}` : 'World backed up');
    onDone();
  } catch (err) {
    toast(`Could not back up the world: ${errMessage(err)}`, 'error');
  }
}

export function WorldsPane({
  inst,
  resources,
  onRefresh,
}: {
  inst: EnrichedInstance;
  resources: ResourceLoadState;
  onRefresh: () => void;
}): JSX.Element {
  const worlds = resources.data?.worlds ?? [];
  return (
    <div class="cp-instance-body" style={{ display: 'block' }}>
      <ResourceToolbar
        title={`${worlds.length} world${worlds.length === 1 ? '' : 's'}`}
        onRefresh={onRefresh}
        action={{ icon: 'folder', label: 'Open saves', onClick: () => void openInstanceFolder(inst.id, 'saves') }}
      />
      <ResourceStatus state={resources} onRetry={onRefresh} />
      {worlds.length === 0 && resources.status !== 'loading' ? (
        <ResourceEmpty icon="globe" title="No saves yet" hint="Create a world in Minecraft or place an existing save in this instance's saves folder." />
      ) : (
        <div class="cp-resource-list">
          {worlds.map((world) => (
            <ResourceRow
              key={world.name}
              icon="globe"
              name={world.name}
              meta={`${fmtBytes(world.size)} · changed ${fmtRelative(world.modified_at)}`}
              actions={(
                <button
                  class="cp-resource-action"
                  type="button"
                  aria-label={`World actions for ${world.name}`}
                  onClick={(e) => openContextMenu(e, [
                    { icon: 'edit', label: 'Rename', onSelect: () => void renameWorld(inst, world.name, onRefresh) },
                    { icon: 'archive', label: 'Back up', onSelect: () => void backupWorld(inst, world.name, onRefresh) },
                    { divider: true, label: '', onSelect: () => undefined },
                    { icon: 'trash', label: 'Delete', onSelect: () => void deleteWorld(inst, world.name, onRefresh), danger: true },
                  ])}
                >
                  <Icon name="dots" size={15} />
                </button>
              )}
            />
          ))}
        </div>
      )}
    </div>
  );
}
