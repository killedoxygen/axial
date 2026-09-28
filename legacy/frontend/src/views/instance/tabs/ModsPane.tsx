import type { JSX } from 'preact';
import { contentRevision } from '../../../content-activity';
import { useCallback, useEffect, useRef, useState } from 'preact/hooks';
import { Icon } from '../../../ui/Icons';
import { Button, Input } from '../../../ui/Atoms';
import { SelectField } from '../../../ui/Select';
import { openContextMenu } from '../../../ui/ContextMenu';
import { SelectionActionTray, SelectionCheckbox } from '../../../ui/SelectionActionTray';
import { selectionMenuItem, selectionToggleLabel, useSelection } from '../../../ui/selection';
import { navigate } from '../../../ui-state';
import { formatBytes } from '../../../format';
import type { EnrichedInstance, InstanceMod } from '../../../types-instance';
import { modBaseName } from '../../../utils';
import type { ResourceLoadState } from '../resources';
import { openInstanceFolder } from '../instance-actions';
import { ResourceStatus } from '../components/resource-bits';
import {
  applyModUpdates,
  cachedModProvenance,
  deleteMods,
  fetchModProvenance,
  modMenuItems,
  setModsEnabled,
  type ModProvenance,
} from '../mod-actions';

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
  const [provenance, setProvenance] = useState<ModProvenance | null>(() => cachedModProvenance(inst.id));
  const [provenanceStamp, setProvenanceStamp] = useState(0);
  const resourceRevision = useRef({ instanceId: inst.id, revision: contentRevision.value });

  useEffect(() => {
    let alive = true;
    setProvenance(cachedModProvenance(inst.id));
    void fetchModProvenance(inst.id, (data) => {
      if (alive) setProvenance(data);
    }).catch(() => {});
    return () => {
      alive = false;
    };
  }, [inst.id, provenanceStamp, contentRevision.value]);

  useEffect(() => {
    const observed = resourceRevision.current;
    if (observed.instanceId !== inst.id) {
      resourceRevision.current = { instanceId: inst.id, revision: contentRevision.value };
      return;
    }
    if (observed.revision === contentRevision.value) return;
    resourceRevision.current = { instanceId: inst.id, revision: contentRevision.value };
    onRefresh();
  }, [inst.id, onRefresh, contentRevision.value]);

  const refreshAll = (): void => {
    setProvenanceStamp((stamp) => stamp + 1);
    onRefresh();
  };

  const provenanceFor = (mod: InstanceMod) => {
    const entry = provenance?.entries.get(modBaseName(mod.name));
    const update = entry ? provenance?.updates.get(entry.canonical_id) : undefined;
    return { entry, update };
  };

  const mods = resources.data?.mods ?? [];
  const filteredMods = mods.filter((mod) => {
    const needle = q.trim().toLowerCase();
    const title = provenanceFor(mod).entry?.title ?? '';
    const matchesSearch = mod.name.toLowerCase().includes(needle) || title.toLowerCase().includes(needle);
    const matchesFilter = filter === 'all' || (filter === 'enabled' ? mod.enabled : !mod.enabled);
    return matchesSearch && matchesFilter;
  });
  const pendingUpdates = provenance ? [...provenance.updates.values()] : [];
  const selection = useSelection(
    filteredMods,
    useCallback((mod: InstanceMod) => mod.name, []),
  );
  const selectedMods = selection.selectedItems;
  const allSelectedEnabled = selectedMods.length > 0 && selectedMods.every((mod) => mod.enabled);
  const allSelectedDisabled = selectedMods.length > 0 && selectedMods.every((mod) => !mod.enabled);
  const clearAndRefresh = (): void => {
    selection.clear();
    refreshAll();
  };

  return (
    <div class="cp-instance-body">
      <div class="cp-resource-toolbar">
        <div>
          <strong>
            {mods.length} mod{mods.length === 1 ? '' : 's'}
          </strong>
          <Input
            value={q}
            onChange={setQ}
            placeholder="Filter mods…"
            icon="search"
            style={{ width: 200, height: 30 }}
          />
        </div>
        <div>
          <SelectField
            value={filter}
            onChange={setFilter}
            options={[
              { value: 'all', label: 'All mods' },
              { value: 'enabled', label: 'Enabled' },
              { value: 'disabled', label: 'Disabled' },
            ]}
            ariaLabel="Filter mods by state"
            width={116}
          />
          <Button variant="secondary" size="sm" icon="refresh" onClick={refreshAll}>
            Refresh
          </Button>
          <Button variant="secondary" size="sm" icon="folder" onClick={() => void openInstanceFolder(inst.id, 'mods')}>
            Open folder
          </Button>
          <Button
            variant="soft"
            size="sm"
            icon="compass"
            onClick={() => navigate({ name: 'discover', target: inst.id })}
          >
            Add content
          </Button>
        </div>
      </div>
      {pendingUpdates.length > 0 && (
        <div class="cp-mods-updates" role="status">
          <Icon name="arrow-up" size={14} color="var(--accent)" />
          <span>
            {pendingUpdates.length} update{pendingUpdates.length === 1 ? '' : 's'} available
          </span>
          <span class="cp-mods-updates-action">
            <Button size="sm" onClick={() => void applyModUpdates(inst, pendingUpdates)}>
              Update all
            </Button>
          </span>
        </div>
      )}
      <ResourceStatus state={resources} onRetry={onRefresh} />
      <div class="cp-mods-table">
        <div class="cp-mods-table-head" aria-hidden="true">
          <span />
          <span />
          <span />
          <span>Name</span>
          <span>Source</span>
          <span>Size</span>
          <span>State</span>
          <span />
        </div>
        {resources.status !== 'loading' && filteredMods.length === 0 ? (
          <div class="cp-mods-empty-row">
            <strong>{mods.length === 0 ? 'No mods installed in this instance' : 'No mods match this filter'}</strong>
            {mods.length === 0 ? (
              <>
                Browse Discover to add mods that fit {inst.version_display.summary_label}, or drop jar files straight
                into the mods folder.
                <div class="cp-mods-empty-actions">
                  <Button size="sm" icon="compass" onClick={() => navigate({ name: 'discover', target: inst.id })}>
                    Browse Discover
                  </Button>
                </div>
              </>
            ) : (
              'Try a different filter.'
            )}
          </div>
        ) : (
          filteredMods.map((mod) => {
            const { entry, update } = provenanceFor(mod);
            return (
              <div
                class="cp-mods-table-row cp-selection-row"
                data-disabled={!mod.enabled}
                data-selected={selection.isSelected(mod.name)}
                key={mod.name}
                onContextMenu={(e) =>
                  openContextMenu(
                    e,
                    modMenuItems(inst, mod, refreshAll, selectionMenuItem(selection, mod.name), { entry, update }),
                  )
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
                <span class="cp-mod-name-cell">
                  <span class="cp-resource-name" title={mod.name}>
                    {entry?.title ?? mod.name}
                  </span>
                  {update && (
                    <button
                      type="button"
                      class="cp-mod-update"
                      title={`Update to ${update.latest_version_number}`}
                      onClick={(e) => {
                        e.stopPropagation();
                        void applyModUpdates(inst, [update]);
                      }}
                    >
                      <Icon name="arrow-up" size={11} stroke={2.4} />
                      Update
                    </button>
                  )}
                </span>
                <span class="cp-mod-source" data-provider={entry ? entry.provider : 'local'}>
                  {entry ? 'Modrinth' : 'Local'}
                </span>
                <span>{formatBytes(mod.size)}</span>
                <span>{mod.enabled ? 'Enabled' : 'Disabled'}</span>
                <span />
              </div>
            );
          })
        )}
      </div>
      <SelectionActionTray
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
            onClick: () => void deleteMods(inst, selectedMods, clearAndRefresh, provenance?.entries),
          },
        ]}
      />
    </div>
  );
}
