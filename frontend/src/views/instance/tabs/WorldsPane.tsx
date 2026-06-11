import type { JSX } from 'preact';
import { Icon } from '../../../ui/Icons';
import { openContextMenu } from '../../../ui/ContextMenu';
import type { EnrichedInstance } from '../../../types';
import { fmtBytes, fmtRelative } from '../format';
import type { ResourceLoadState } from '../resources';
import { openInstanceFolder } from '../instance-actions';
import { ResourceEmpty, ResourceRow, ResourceStatus, ResourceToolbar } from '../components/resource-bits';
import { worldMenuItems } from '../world-actions';

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
              onContextMenu={(e) => openContextMenu(e, worldMenuItems(inst, world.name, onRefresh))}
              actions={(
                <button
                  class="cp-resource-action"
                  type="button"
                  aria-label={`World actions for ${world.name}`}
                  onClick={(e) => openContextMenu(e, worldMenuItems(inst, world.name, onRefresh))}
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
