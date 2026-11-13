import type { JSX } from 'preact';
import { useMemo } from 'preact/hooks';
import { mediaTicketEpoch } from '../../../api';
import { Button } from '../../../ui/Atoms';
import { formatBytes, fmtRelative } from '../../../format';
import type { EnrichedInstance } from '../../../types-instance';
import { fileArtUrl, type ResourceLoadState } from '../resources';
import { openInstanceFolder } from '../instance-actions';
import { EntryPane, type EntryView } from '../components/entries';
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
  const busy = resourceMutationState(inst.id).status === 'pending';
  const worlds = resources.data?.worlds;
  const rows = useMemo(
    () =>
      (worlds ?? []).map((world): EntryView => ({
        key: world.name,
        title: world.name,
        byline: `Changed ${fmtRelative(world.modified_at)}`,
        num: { text: formatBytes(world.size) },
        artUrl: fileArtUrl(inst.id, 'worlds', world.name, world.modified_at),
        icon: 'globe',
      })),
    [worlds, inst.id, mediaTicketEpoch],
  );
  const openSaves = (): void => void openInstanceFolder(inst.id, 'saves');

  return (
    <EntryPane
      instanceId={inst.id}
      noun="world"
      rows={rows}
      state={resources}
      expected={resources.data?.worlds_count ?? inst.saves_count}
      onRefresh={onRefresh}
      onOpenFolder={openSaves}
      folderTip="Open saves"
      empty={{
        title: 'No worlds yet',
        hint: 'Create a new world, import an existing save, or launch Minecraft and create one there.',
        actions: (
          <Button variant="secondary" icon="folder" onClick={openSaves}>
            Import world
          </Button>
        ),
      }}
      menu={(key, selectionItem) => [
        selectionItem,
        { divider: true, label: '', onSelect: () => undefined },
        ...worldMenuItems(inst, key, onRefresh),
      ]}
      actions={(selected, clear) => [
        {
          label: 'Delete',
          icon: 'trash',
          danger: true,
          disabled: busy,
          onClick: () =>
            void deleteWorlds(
              inst,
              selected.map((view) => view.key),
              () => {
                clear();
                onRefresh();
              },
              onRefresh,
            ),
        },
      ]}
    />
  );
}
