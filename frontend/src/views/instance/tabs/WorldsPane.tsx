import type { JSX } from 'preact';
import { useCallback } from 'preact/hooks';
import { Button, Empty, IconButton } from '../../../ui/Atoms';
import { Art } from '../../../ui/Art';
import { openContextMenu } from '../../../ui/ContextMenu';
import { SelectionActionTray } from '../../../ui/SelectionActionTray';
import { selectionMenuItem, useSelection } from '../../../ui/selection';
import { formatBytes, fmtRelative } from '../../../format';
import type { EnrichedInstance } from '../../../types-instance';
import type { ResourceLoadState } from '../resources';
import { openInstanceFolder } from '../instance-actions';
import { ResourceMutationStatus, ResourceStatus } from '../components/resource-bits';
import { Entry } from '../components/content-row';
import { deleteWorlds, worldMenuItems } from '../world-actions';
import { resourceMutationState } from '../bulk-actions';

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
  const busy = resourceMutationState(inst.id).status === 'pending';
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
      onRefresh,
    );
  };
  const clearAndRefresh = (): void => {
    selection.clear();
    onRefresh();
  };

  const openSaves = (): void => void openInstanceFolder(inst.id, 'saves');
  const empty = worlds.length === 0 && resources.status === 'ready';

  return (
    <div class="cp-pane">
      {!empty && (
        <div class="cp-pane-bar">
          <Button variant="secondary" icon="folder" onClick={openSaves}>
            Open saves
          </Button>
          <IconButton icon="refresh" tooltip="Refresh" onClick={onRefresh} />
        </div>
      )}
      <ResourceStatus state={resources} onRetry={onRefresh} />
      <ResourceMutationStatus instanceId={inst.id} />
      {empty ? (
        <Empty
          title="No worlds yet"
          actions={
            <Button variant="secondary" icon="folder" onClick={openSaves}>
              Import world
            </Button>
          }
        >
          Create a new world, import an existing save, or launch Minecraft and create one there.
        </Empty>
      ) : (
        <div class="cp-entries">
          {worlds.map((world) => (
            <Entry
              key={world.name}
              art={<Art icon="globe" />}
              title={world.name}
              byline={`Changed ${fmtRelative(world.modified_at)}`}
              num={formatBytes(world.size)}
              selected={selection.isSelected(world.name)}
              onToggle={() => selection.toggle(world.name)}
              onMenu={(e) => openContextMenu(e, menuItems(world.name))}
              menuLabel={`World actions for ${world.name}`}
            />
          ))}
        </div>
      )}
      <SelectionActionTray
        selection={selection}
        itemLabel="world"
        actions={[
          { label: 'Delete', icon: 'trash', danger: true, disabled: busy, onClick: () => void deleteSelected() },
        ]}
      />
    </div>
  );
}
