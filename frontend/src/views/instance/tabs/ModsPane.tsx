import type { JSX } from 'preact';
import { contentRevision } from '../../../content-activity';
import { useCallback, useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { Icon } from '../../../ui/Icons';
import { Button, Input } from '../../../ui/Atoms';
import { SelectField } from '../../../ui/Select';
import { openContextMenu } from '../../../ui/ContextMenu';
import { SelectionActionTray } from '../../../ui/SelectionActionTray';
import { selectionMenuItem, useSelection } from '../../../ui/selection';
import { navigate } from '../../../ui-state';
import type { EnrichedInstance, InstanceMod } from '../../../types-instance';
import type { ContentFileMetadata } from '../../../generated/ContentFileMetadata';
import { modBaseName } from '../../../utils';
import { fetchModMetadata, modIconUrl, type ResourceLoadState } from '../resources';
import { openInstanceFolder } from '../instance-actions';
import { ResourceMutationStatus, ResourceStatus } from '../components/resource-bits';
import {
  ContentRow,
  compareContentRows,
  contentRowMatches,
  fileDisplayName,
  type ContentRowView,
} from '../components/content-row';
import { PackOrigin, packMembership } from '../components/pack-origin';
import { useModImport } from '../components/mod-import';
import { resourceMutationState } from '../bulk-actions';
import {
  applyModUpdates,
  bulkModUpdates,
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
  const busy = resourceMutationState(inst.id).status === 'pending';
  const [filter, setFilter] = useState<ModFilter>('all');
  const [provenance, setProvenance] = useState<ModProvenance | null>(() => cachedModProvenance(inst.id));
  const [provenanceError, setProvenanceError] = useState('');
  const [provenanceStamp, setProvenanceStamp] = useState(0);
  const [metadata, setMetadata] = useState<Map<string, ContentFileMetadata>>(() => new Map());
  const resourceRevision = useRef({ instanceId: inst.id, revision: contentRevision.value });
  const dropSurfaceRef = useRef<HTMLDivElement | null>(null);
  const modImport = useModImport(inst.id, dropSurfaceRef, onRefresh);

  // Jar metadata is best-effort display text; rows fall back to filenames without it.
  useEffect(() => {
    let alive = true;
    void fetchModMetadata(inst.id)
      .then((next) => {
        if (alive) setMetadata(next);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [inst.id, provenanceStamp, contentRevision.value]);

  useEffect(() => {
    let alive = true;
    setProvenance(cachedModProvenance(inst.id));
    setProvenanceError('');
    void fetchModProvenance(inst.id, (data) => {
      if (alive) setProvenance(data);
    }).catch(() => {
      if (alive) setProvenanceError('Could not load mod details. Refresh to try again.');
    });
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

  function modRow(mod: InstanceMod): ContentRowView {
    const { entry, update } = provenanceFor(mod);
    const details = metadata.get(mod.name);
    return {
      key: mod.name,
      title: entry?.title ?? details?.title ?? fileDisplayName(mod.name),
      fileName: mod.name,
      details: [details?.version, details?.authors.join(', ')].filter((text): text is string => Boolean(text)),
      iconUrl: details?.icon ? modIconUrl(inst.id, mod.name) : undefined,
      fallbackIcon: 'puzzle',
      managed: Boolean(entry),
      pack: membership.get(`mods/${modBaseName(mod.name)}`),
      enabled: mod.enabled,
      size: mod.size,
      update: update && {
        version: update.latest_version_number,
        disabled: busy,
        onClick: () => void applyModUpdates(inst, [update]),
      },
    };
  }

  const mods = resources.data?.mods ?? [];
  const packs = provenance?.packs ?? [];
  const membership = useMemo(() => packMembership(packs), [packs]);
  const rows = mods
    .map((mod) => ({ mod, view: modRow(mod) }))
    .sort((left, right) => compareContentRows(left.view, right.view));
  const filteredRows = rows.filter(
    ({ mod, view }) =>
      contentRowMatches(view, q) && (filter === 'all' || (filter === 'enabled' ? mod.enabled : !mod.enabled)),
  );
  const filteredMods = filteredRows.map(({ mod }) => mod);
  const pendingUpdates = provenance ? [...provenance.updates.values()].filter((update) => update.kind === 'mod') : [];
  const bulkUpdates = bulkModUpdates(pendingUpdates);
  const packKept = pendingUpdates.length - bulkUpdates.length;
  const packTitle = pendingUpdates.find((update) => update.pack)?.pack?.title ?? 'the modpack';
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
    <div
      class="cp-instance-body"
      ref={dropSurfaceRef}
      data-drop-state={modImport.dropActive ? 'active' : 'idle'}
      {...modImport.dropHandlers}
    >
      {modImport.input}
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
          <Button variant="secondary" size="sm" icon="plus" disabled={modImport.busy} onClick={modImport.pick}>
            Add jar files
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
      <PackOrigin packs={packs} instanceId={inst.id} />
      {pendingUpdates.length > 0 && (
        <div class="cp-mods-updates" role="status">
          <Icon name="arrow-up" size={14} color="var(--accent)" />
          <span>
            {pendingUpdates.length} update{pendingUpdates.length === 1 ? '' : 's'} available
            {packKept > 0 &&
              `. ${bulkUpdates.length === 0 ? (packKept === 1 ? 'It' : 'All') : packKept} came with ${packTitle} and ${
                packKept === 1 ? 'stays' : 'stay'
              } with it`}
          </span>
          {bulkUpdates.length > 0 && (
            <span class="cp-mods-updates-action">
              <Button size="sm" disabled={busy} onClick={() => void applyModUpdates(inst, bulkUpdates)}>
                {packKept > 0 ? `Update ${bulkUpdates.length}` : 'Update all'}
              </Button>
            </span>
          )}
        </div>
      )}
      <ResourceStatus state={resources} onRetry={onRefresh} />
      <ResourceMutationStatus instanceId={inst.id} />
      {provenanceError || provenance?.updateError ? (
        <div class="cp-resource-note cp-resource-note--error" role="alert">
          {provenanceError || provenance?.updateError}
        </div>
      ) : null}
      <div class="cp-content-list" data-drop-state={modImport.dropActive ? 'active' : 'idle'}>
        {resources.status === 'ready' && filteredRows.length === 0 ? (
          <div class="cp-mods-empty-row">
            <strong>{mods.length === 0 ? 'No mods installed in this instance' : 'No mods match this filter'}</strong>
            {mods.length === 0 ? (
              <>
                {modImport.dropActive
                  ? 'Drop to add.'
                  : `Browse Discover to add mods that fit ${inst.version_display.summary_label}, or add jar files from your computer.`}
                <div class="cp-mods-empty-actions">
                  <Button size="sm" icon="compass" onClick={() => navigate({ name: 'discover', target: inst.id })}>
                    Browse Discover
                  </Button>
                  <Button variant="secondary" size="sm" icon="plus" disabled={modImport.busy} onClick={modImport.pick}>
                    Add jar files
                  </Button>
                </div>
              </>
            ) : (
              'Try a different filter.'
            )}
          </div>
        ) : (
          filteredRows.map(({ mod, view }) => (
            <ContentRow
              key={mod.name}
              view={view}
              selected={selection.isSelected(mod.name)}
              onToggle={() => selection.toggle(mod.name)}
              onMenu={(e) =>
                openContextMenu(
                  e,
                  modMenuItems(inst, mod, refreshAll, selectionMenuItem(selection, mod.name), provenanceFor(mod)),
                )
              }
            />
          ))
        )}
      </div>
      <SelectionActionTray
        selection={selection}
        itemLabel="mod"
        actions={[
          {
            label: 'Enable',
            icon: 'play',
            disabled: busy || allSelectedEnabled,
            onClick: () => void setModsEnabled(inst, selectedMods, true, clearAndRefresh, refreshAll),
          },
          {
            label: 'Disable',
            icon: 'stop',
            disabled: busy || allSelectedDisabled,
            onClick: () => void setModsEnabled(inst, selectedMods, false, clearAndRefresh, refreshAll),
          },
          {
            label: 'Delete',
            icon: 'trash',
            danger: true,
            disabled: busy,
            onClick: () => void deleteMods(inst, selectedMods, clearAndRefresh, provenance?.entries, refreshAll),
          },
        ]}
      />
    </div>
  );
}
