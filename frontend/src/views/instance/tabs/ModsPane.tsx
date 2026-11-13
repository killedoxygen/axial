import type { JSX } from 'preact';
import { useMemo, useRef, useState } from 'preact/hooks';
import { mediaTicketEpoch } from '../../../api';
import { Button, IconButton } from '../../../ui/Atoms';
import { SelectField } from '../../../ui/Select';
import { navigate } from '../../../ui-state';
import type { ContentUpdate, InstanceContentEntry } from '../../../types-content';
import type { EnrichedInstance, InstanceMod } from '../../../types-instance';
import { modBaseName } from '../../../utils';
import { fetchModMetadata, fileArtUrl, type LoadState, type ResourceLoadState } from '../resources';
import { useListing, useModProvenance } from '../listings';
import { openInstanceFolder } from '../instance-actions';
import { EntryPane, compareEntries, fileDisplayName, type EntryView } from '../components/entries';
import { packMembership } from '../components/pack-origin';
import { useModImport } from '../components/mod-import';
import { resourceMutationState } from '../bulk-actions';
import { applyModUpdates, bulkModUpdates, deleteMods, modMenuItems, setModsEnabled, updateFor } from '../mod-actions';
import { updateLabel, versionLabel } from '../version-label';

type ModFilter = 'all' | 'enabled' | 'disabled' | 'updates';

const FILTERS: Record<Exclude<ModFilter, 'all'>, (view: EntryView) => boolean> = {
  enabled: (view) => !view.disabled,
  disabled: (view) => view.disabled === true,
  updates: (view) => view.update !== undefined,
};

interface ModRow {
  mod: InstanceMod;
  entry?: InstanceContentEntry;
  update?: ContentUpdate;
}

const WAITING: LoadState<never> = { status: 'loading', data: null };

export function ModsPane({
  inst,
  resources,
  onRefresh,
}: {
  inst: EnrichedInstance;
  resources: ResourceLoadState;
  onRefresh: () => void;
}): JSX.Element {
  const busy = resourceMutationState(inst.id).status === 'pending';
  const [filter, setFilter] = useState<ModFilter>('all');
  const [stamp, setStamp] = useState(0);
  const { provenance, error: provenanceError } = useModProvenance(inst.id, stamp);
  // Jar metadata is best-effort display text; rows fall back to filenames without it.
  const metadata = useListing(inst.id, 'mods/metadata', () => fetchModMetadata(inst.id));
  const dropSurfaceRef = useRef<HTMLDivElement | null>(null);
  const modImport = useModImport(inst.id, dropSurfaceRef, onRefresh);

  const refreshAll = (): void => {
    setStamp((value) => value + 1);
    metadata.refresh();
    onRefresh();
  };

  // A row is named by its install record, then its jar, then its file. The list waits for the first two so no
  // row is renamed, and so moved, once it is on screen.
  const named =
    (metadata.data !== null || metadata.status !== 'loading') && (provenance !== null || provenanceError !== '');
  const mods = resources.data?.mods;
  const game = inst.version_display.minecraft_label;
  const { rows, byKey } = useMemo(() => {
    const byKey = new Map<string, ModRow>();
    const membership = packMembership(provenance?.packs ?? []);
    const rows = (named ? (mods ?? []) : []).map((mod): EntryView => {
      const base = modBaseName(mod.name);
      const entry = provenance?.entries.get(base);
      const update = updateFor(provenance, entry);
      const details = metadata.data?.get(mod.name);
      const num = details?.version ? versionLabel(details.version, game) : undefined;
      byKey.set(mod.name, { mod, entry, update });
      return {
        key: mod.name,
        title: entry?.title ?? details?.title ?? fileDisplayName(mod.name),
        fileName: mod.name,
        byline: details?.authors.join(', ') || undefined,
        num,
        artUrl: details?.icon ? fileArtUrl(inst.id, 'mods', mod.name, mod.modified_at) : undefined,
        icon: 'puzzle',
        pack: membership.get(`mods/${base}`),
        disabled: !mod.enabled,
        update: update && updateLabel(update.latest_version_number, game, update.latest_channel, num),
      };
    });
    return { rows: rows.sort(compareEntries), byKey };
  }, [named, mods, metadata.data, provenance, game, inst.id, mediaTicketEpoch]);

  const pending = useMemo(() => [...byKey.values()].flatMap((row) => (row.update ? [row.update] : [])), [byKey]);
  const bulkUpdates = bulkModUpdates(pending);
  const packKept = pending.length - bulkUpdates.length;
  const packTitle = pending.find((update) => update.pack)?.pack?.title ?? 'the modpack';
  const browse = (): void => navigate({ name: 'discover', target: inst.id });

  return (
    <EntryPane
      instanceId={inst.id}
      noun="mod"
      rows={rows}
      state={resources.status === 'ready' && !named ? WAITING : resources}
      expected={resources.data?.mods_count ?? inst.mods_count}
      onRefresh={refreshAll}
      onOpenFolder={() => void openInstanceFolder(inst.id, 'mods')}
      filter={
        <SelectField
          value={filter}
          onChange={setFilter}
          options={[
            { value: 'all', label: 'All mods' },
            { value: 'enabled', label: 'Enabled' },
            { value: 'disabled', label: 'Disabled' },
            { value: 'updates', label: 'Updates' },
          ]}
          ariaLabel="Filter mods"
        />
      }
      shown={filter === 'all' ? undefined : FILTERS[filter]}
      updates={{
        count: bulkUpdates.length,
        note:
          packKept > 0
            ? `${packKept} more came with ${packTitle} and ${packKept === 1 ? 'stays' : 'stay'} with it`
            : undefined,
        all: () => void applyModUpdates(inst, bulkUpdates),
        one: (key) => {
          const update = byKey.get(key)?.update;
          if (update) void applyModUpdates(inst, [update]);
        },
      }}
      tools={
        <>
          <IconButton icon="archive" tooltip="Add jar files" disabled={modImport.busy} onClick={modImport.pick} />
          <Button variant="soft" icon="plus" onClick={browse}>
            Add mods
          </Button>
        </>
      }
      note={provenanceError || provenance?.updateError}
      empty={{
        title: 'No mods installed in this instance',
        hint: modImport.dropActive
          ? 'Drop to add.'
          : `Browse Discover to add mods that fit ${inst.version_display.summary_label}, or add jar files from your computer.`,
        actions: (
          <>
            <Button variant="secondary" icon="compass" onClick={browse}>
              Browse Discover
            </Button>
            <Button variant="secondary" icon="archive" disabled={modImport.busy} onClick={modImport.pick}>
              Add jar files
            </Button>
          </>
        ),
      }}
      menu={(key, selectionItem) => {
        const row = byKey.get(key);
        return row ? modMenuItems(inst, row.mod, refreshAll, selectionItem, row) : [];
      }}
      actions={(selected, clear) => {
        const chosen = selected.flatMap((view) => byKey.get(view.key)?.mod ?? []);
        const clearAndRefresh = (): void => {
          clear();
          refreshAll();
        };
        return [
          {
            label: 'Enable',
            icon: 'play',
            disabled: busy || chosen.every((mod) => mod.enabled),
            onClick: () => void setModsEnabled(inst, chosen, true, clearAndRefresh, refreshAll),
          },
          {
            label: 'Disable',
            icon: 'stop',
            disabled: busy || chosen.every((mod) => !mod.enabled),
            onClick: () => void setModsEnabled(inst, chosen, false, clearAndRefresh, refreshAll),
          },
          {
            label: 'Delete',
            icon: 'trash',
            danger: true,
            disabled: busy,
            onClick: () => void deleteMods(inst, chosen, clearAndRefresh, provenance?.entries, refreshAll),
          },
        ];
      }}
      surface={{
        ref: dropSurfaceRef,
        'data-drop-state': modImport.dropActive ? 'active' : 'idle',
        ...modImport.dropHandlers,
      }}
    >
      {modImport.input}
    </EntryPane>
  );
}
