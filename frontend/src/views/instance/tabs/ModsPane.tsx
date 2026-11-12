import type { JSX } from 'preact';
import { contentRevision } from '../../../content-activity';
import { useCallback, useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { Button, Empty, IconButton, Input } from '../../../ui/Atoms';
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
  UpdateAll,
  compareContentRows,
  contentRowMatches,
  fileDisplayName,
  type ContentRowView,
} from '../components/content-row';
import { packMembership } from '../components/pack-origin';
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
import { settleChildren } from '../../../ui/motion';

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
      version: details?.version,
      details: details?.authors.length ? [details.authors.join(', ')] : [],
      iconUrl: details?.icon ? modIconUrl(inst.id, mod.name) : undefined,
      fallbackIcon: 'puzzle',
      pack: membership.get(`mods/${modBaseName(mod.name)}`),
      enabled: mod.enabled,
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
    .sort(
      (left, right) =>
        Number(Boolean(right.view.update)) - Number(Boolean(left.view.update)) ||
        compareContentRows(left.view, right.view),
    );
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

  const empty = resources.status === 'ready' && mods.length === 0;
  const browse = (): void => navigate({ name: 'discover', target: inst.id });

  return (
    <div
      class="cp-pane"
      ref={dropSurfaceRef}
      data-drop-state={modImport.dropActive ? 'active' : 'idle'}
      {...modImport.dropHandlers}
    >
      {modImport.input}
      {!empty && (
        <div class="cp-pane-bar">
          <Input value={q} onChange={setQ} placeholder="Search mods" icon="search" />
          <SelectField
            value={filter}
            onChange={setFilter}
            options={[
              { value: 'all', label: 'All mods' },
              { value: 'enabled', label: 'Enabled' },
              { value: 'disabled', label: 'Disabled' },
            ]}
            ariaLabel="Filter mods by state"
          />
          {bulkUpdates.length > 0 && (
            <UpdateAll
              count={bulkUpdates.length}
              noun="mod"
              note={
                packKept > 0
                  ? `${packKept} more came with ${packTitle} and ${packKept === 1 ? 'stays' : 'stay'} with it`
                  : undefined
              }
              disabled={busy}
              onClick={() => void applyModUpdates(inst, bulkUpdates)}
            />
          )}
          <span class="cp-head-fill" />
          <IconButton icon="refresh" tooltip="Refresh" onClick={refreshAll} />
          <IconButton icon="folder" tooltip="Open folder" onClick={() => void openInstanceFolder(inst.id, 'mods')} />
          <IconButton icon="archive" tooltip="Add jar files" disabled={modImport.busy} onClick={modImport.pick} />
          <Button variant="soft" icon="plus" onClick={browse}>
            Add mods
          </Button>
        </div>
      )}
      <ResourceStatus state={resources} onRetry={onRefresh} />
      <ResourceMutationStatus instanceId={inst.id} />
      {provenanceError || provenance?.updateError ? (
        <div class="cp-resource-note cp-resource-note--error" role="alert">
          {provenanceError || provenance?.updateError}
        </div>
      ) : null}
      {empty ? (
        <Empty
          title="No mods installed in this instance"
          actions={
            <>
              <Button variant="secondary" icon="compass" onClick={browse}>
                Browse Discover
              </Button>
              <Button variant="secondary" icon="archive" disabled={modImport.busy} onClick={modImport.pick}>
                Add jar files
              </Button>
            </>
          }
        >
          {modImport.dropActive
            ? 'Drop to add.'
            : `Browse Discover to add mods that fit ${inst.version_display.summary_label}, or add jar files from your computer.`}
        </Empty>
      ) : resources.status === 'ready' && filteredRows.length === 0 ? (
        <Empty title="No mods match this filter">Try a different filter.</Empty>
      ) : (
        <div class="cp-entries" data-settles ref={(list) => settleChildren(list)}>
          {filteredRows.map(({ mod, view }) => (
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
          ))}
        </div>
      )}
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
