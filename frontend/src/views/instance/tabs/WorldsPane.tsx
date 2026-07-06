import type { JSX } from 'preact';
import { useCallback } from 'preact/hooks';
import { Icon } from '../../../ui/Icons';
import { Button } from '../../../ui/Atoms';
import { openContextMenu } from '../../../ui/ContextMenu';
import { SelectionActionPill, SelectionCheckbox } from '../../../ui/SelectionActionPill';
import { selectionMenuItem, selectionToggleLabel, useSelection } from '../../../ui/selection';
import type { EnrichedInstance } from '../../../types-instance';
import { fmtBytes, fmtRelative } from '../format';
import type { ResourceLoadState } from '../resources';
import { openInstanceFolder } from '../instance-actions';
import { ResourceRow, ResourceStatus, ResourceToolbar } from '../components/resource-bits';
import { WorldsEmptyArt } from '../components/worlds-empty-art';
import { deleteWorlds, worldMenuItems } from '../world-actions';

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
  const selection = useSelection(
    worlds,
    useCallback((world) => world.name, []),
  );
  const menuItems = (worldName: string) => [
    selectionMenuItem(selection, worldName),
    { divider: true, label: '', onSelect: () => undefined },
    ...worldMenuItems(inst, worldName, onRefresh),
  ];
  const deleteSelected = async (): Promise<void> => {
    await deleteWorlds(
      inst,
      selection.selectedItems.map((world) => world.name),
      clearAndRefresh,
    );
  };
  const clearAndRefresh = (): void => {
    selection.clear();
    onRefresh();
  };

  return (
    <div class="cp-instance-body">
      <ResourceToolbar
        title={`${worlds.length} world${worlds.length === 1 ? '' : 's'}`}
        onRefresh={onRefresh}
        action={{ icon: 'folder', label: 'Open saves', onClick: () => void openInstanceFolder(inst.id, 'saves') }}
      />
      <ResourceStatus state={resources} onRetry={onRefresh} />
      {worlds.length === 0 && resources.status !== 'loading' ? (
        <div class="cp-resource-empty cp-worlds-empty">
          <WorldsEmptyArt />
          <strong>No worlds yet</strong>
          <p>Create a new world, import an existing save, or launch Minecraft and create one there.</p>
          <div class="cp-worlds-empty-actions">
            <Button variant="secondary" icon="folder" onClick={() => void openInstanceFolder(inst.id, 'saves')}>
              Import world
            </Button>
          </div>
        </div>
      ) : (
        <div class="cp-resource-list">
          {worlds.map((world) => (
            <ResourceRow
              key={world.name}
              icon="globe"
              name={world.name}
              meta={`${fmtBytes(world.size)} · changed ${fmtRelative(world.modified_at)}`}
              selected={selection.isSelected(world.name)}
              leading={
                <SelectionCheckbox
                  selected={selection.isSelected(world.name)}
                  label={selectionToggleLabel(selection.isSelected(world.name), world.name)}
                  onToggle={(e) => {
                    e.stopPropagation();
                    selection.toggle(world.name);
                  }}
                />
              }
              onContextMenu={(e) => openContextMenu(e, menuItems(world.name))}
              actions={
                <button
                  class="cp-resource-action"
                  type="button"
                  aria-label={`World actions for ${world.name}`}
                  onClick={(e) => openContextMenu(e, menuItems(world.name))}
                >
                  <Icon name="dots" size={15} />
                </button>
              }
            />
          ))}
        </div>
      )}
      <SelectionActionPill
        selection={selection}
        itemLabel="world"
        actions={[{ label: 'Delete', icon: 'trash', danger: true, onClick: () => void deleteSelected() }]}
      />
    </div>
  );
}
