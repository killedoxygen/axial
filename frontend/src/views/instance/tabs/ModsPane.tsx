import type { JSX } from 'preact';
import { useCallback, useState } from 'preact/hooks';
import { Icon } from '../../../ui/Icons';
import { Button, Input } from '../../../ui/Atoms';
import { openContextMenu } from '../../../ui/ContextMenu';
import { SelectionActionPill, SelectionCheckbox } from '../../../ui/SelectionActionPill';
import { selectionMenuItem, selectionToggleLabel, useSelection } from '../../../ui/selection';
import type { EnrichedInstance, InstanceMod } from '../../../types';
import { fmtBytes } from '../format';
import type { ResourceLoadState } from '../resources';
import { openInstanceFolder } from '../instance-actions';
import { ResourceStatus } from '../components/resource-bits';
import { deleteMods, modMenuItems, setModsEnabled } from '../mod-actions';

type ModFilter = 'all' | 'enabled' | 'disabled';

export function ModsPane({
  inst,
  resources,
  onRefresh,
}: {
  inst: EnrichedInstance;
  resources: ResourceLoadState;
  onRefresh: () => void;
}): JSX.Element {
  const [q, setQ] = useState('');
  const [filter, setFilter] = useState<ModFilter>('all');
  const mods = resources.data?.mods ?? [];
  const filteredMods = mods.filter((mod) => {
    const matchesSearch = mod.name.toLowerCase().includes(q.trim().toLowerCase());
    const matchesFilter = filter === 'all' || (filter === 'enabled' ? mod.enabled : !mod.enabled);
    return matchesSearch && matchesFilter;
  });
  const selection = useSelection(
    filteredMods,
    useCallback((mod: InstanceMod) => mod.name, []),
  );
  const selectedMods = selection.selectedItems;
  const allSelectedEnabled = selectedMods.length > 0 && selectedMods.every((mod) => mod.enabled);
  const allSelectedDisabled = selectedMods.length > 0 && selectedMods.every((mod) => !mod.enabled);
  const clearAndRefresh = (): void => {
    selection.clear();
    onRefresh();
  };

  return (
    <div class="cp-instance-body" style={{ display: 'block' }}>
      <div class="cp-resource-toolbar">
        <strong>
          {mods.length} mod{mods.length === 1 ? '' : 's'}
        </strong>
        <div>
          <Input
            value={q}
            onChange={setQ}
            placeholder="Filter mods…"
            icon="search"
            style={{ width: 200, height: 30 }}
          />
          <div class="cp-mini-seg" role="tablist" aria-label="Filter mods">
            {(['all', 'enabled', 'disabled'] as ModFilter[]).map((f) => (
              <button
                key={f}
                type="button"
                role="tab"
                aria-selected={filter === f}
                data-active={filter === f}
                onClick={() => setFilter(f)}
              >
                {f[0].toUpperCase() + f.slice(1)}
              </button>
            ))}
          </div>
          <Button variant="secondary" size="sm" icon="refresh" onClick={onRefresh}>
            Refresh
          </Button>
          <Button variant="soft" size="sm" icon="plus" onClick={() => void openInstanceFolder(inst.id, 'mods')}>
            Add mod
          </Button>
        </div>
      </div>
      <ResourceStatus state={resources} onRetry={onRefresh} />
      <div class="cp-mods-table">
        <div class="cp-mods-table-head" aria-hidden="true">
          <span />
          <span />
          <span />
          <span>Name</span>
          <span>Category</span>
          <span>Version</span>
          <span>State</span>
          <span />
        </div>
        {resources.status !== 'loading' && filteredMods.length === 0 ? (
          <div class="cp-mods-empty-row">
            <strong>{mods.length === 0 ? 'No mods installed in this instance' : 'No mods match this filter'}</strong>
            Drop jar files into the mods folder. In-app mod browsing and metadata are still backend-team work.
          </div>
        ) : (
          filteredMods.map((mod) => (
            <div
              class="cp-mods-table-row cp-selection-row"
              data-disabled={!mod.enabled}
              data-selected={selection.isSelected(mod.name)}
              key={mod.name}
              onContextMenu={(e) =>
                openContextMenu(e, modMenuItems(inst, mod, onRefresh, selectionMenuItem(selection, mod.name)))
              }
            >
              <span>
                <SelectionCheckbox
                  selected={selection.isSelected(mod.name)}
                  label={selectionToggleLabel(selection.isSelected(mod.name), mod.name)}
                  onToggle={(e) => {
                    e.stopPropagation();
                    selection.toggle(mod.name);
                  }}
                />
              </span>
              <span>
                <Icon name="puzzle" size={15} color="var(--text-dim)" />
              </span>
              <span class="cp-mods-file-icon">JAR</span>
              <span class="cp-resource-name" title={mod.name}>
                {mod.name}
              </span>
              <span>Local</span>
              <span>{fmtBytes(mod.size)}</span>
              <span>{mod.enabled ? 'Enabled' : 'Disabled'}</span>
              <span />
            </div>
          ))
        )}
      </div>
      <SelectionActionPill
        selection={selection}
        itemLabel="mod"
        actions={[
          {
            label: 'Enable',
            icon: 'play',
            disabled: allSelectedEnabled,
            onClick: () => void setModsEnabled(inst, selectedMods, true, clearAndRefresh),
          },
          {
            label: 'Disable',
            icon: 'stop',
            disabled: allSelectedDisabled,
            onClick: () => void setModsEnabled(inst, selectedMods, false, clearAndRefresh),
          },
          {
            label: 'Delete',
            icon: 'trash',
            danger: true,
            onClick: () => void deleteMods(inst, selectedMods, clearAndRefresh),
          },
        ]}
      />
    </div>
  );
}
