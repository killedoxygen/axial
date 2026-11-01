import type { JSX } from 'preact';
import { useCallback, useEffect, useMemo, useState } from 'preact/hooks';
import { contentRevision } from '../../../content-activity';
import { Icon } from '../../../ui/Icons';
import { Button, Input } from '../../../ui/Atoms';
import { openContextMenu } from '../../../ui/ContextMenu';
import { SelectionActionTray } from '../../../ui/SelectionActionTray';
import { selectionMenuItem, useSelection } from '../../../ui/selection';
import { navigate } from '../../../ui-state';
import { errMessage } from '../../../utils';
import type { EnrichedInstance } from '../../../types-instance';
import type { InstancePackInfo } from '../../../generated/InstancePackInfo';
import { fetchPacks, resourcePackIconUrl, type PackFolder } from '../resources';
import { openInstanceFolder } from '../instance-actions';
import { ResourceMutationStatus } from '../components/resource-bits';
import {
  ContentRow,
  compareContentRows,
  contentRowMatches,
  fileDisplayName,
  type ContentRowView,
} from '../components/content-row';
import { packMembership } from '../components/pack-origin';
import { resourceMutationState } from '../bulk-actions';
import { applyModUpdates, cachedModProvenance, fetchModProvenance, type ModProvenance } from '../mod-actions';
import { PACK_NOUN, packMenuItems, removePacks, type PackRow } from '../pack-actions';

const KIND = { resourcepacks: 'resource_pack', shaderpacks: 'shader_pack' } as const;

const EMPTY_HINT: Record<PackFolder, string> = {
  resourcepacks:
    'Find resource packs in Discover, or drop .zip files into the folder. Turn them on in game from Options.',
  shaderpacks:
    'Find shaders in Discover, or drop .zip files into the folder. Shaders need a shader loader such as Iris.',
};

type LoadState =
  | { status: 'loading' | 'ready'; packs: InstancePackInfo[] }
  | { status: 'error'; error: string; packs: InstancePackInfo[] };

export function PacksPane({ inst, folder }: { inst: EnrichedInstance; folder: PackFolder }): JSX.Element {
  const noun = PACK_NOUN[folder];
  const [q, setQ] = useState('');
  const [stamp, setStamp] = useState(0);
  const [state, setState] = useState<LoadState>({ status: 'loading', packs: [] });
  const [provenance, setProvenance] = useState<ModProvenance | null>(() => cachedModProvenance(inst.id));
  const busy = resourceMutationState(inst.id).status === 'pending';

  useEffect(() => {
    let alive = true;
    setState((current) => ({ status: 'loading', packs: current.packs }));
    void fetchPacks(inst.id, folder)
      .then((packs) => {
        if (alive) setState({ status: 'ready', packs });
      })
      .catch((err) => {
        if (alive) setState((current) => ({ status: 'error', error: errMessage(err), packs: current.packs }));
      });
    void fetchModProvenance(inst.id, (data) => {
      if (alive) setProvenance(data);
    }).catch(() => {});
    return () => {
      alive = false;
    };
  }, [inst.id, folder, stamp, contentRevision.value]);

  const refresh = (): void => setStamp((value) => value + 1);
  const membership = useMemo(() => packMembership(provenance?.packs ?? []), [provenance]);
  const managed = useMemo(
    () =>
      new Map(
        (provenance?.managed ?? [])
          .filter((entry) => entry.kind === KIND[folder])
          .map((entry) => [entry.filename, entry]),
      ),
    [provenance, folder],
  );

  const rows = state.packs
    .map((pack): { row: PackRow; view: ContentRowView } => {
      const base = pack.name.replace(/\.disabled$/i, '');
      const entry = pack.directory ? undefined : managed.get(base);
      const update = entry ? provenance?.updates.get(entry.canonical_id) : undefined;
      return {
        row: { pack, entry, update },
        view: {
          key: pack.name,
          title: entry?.title ?? fileDisplayName(pack.name),
          fileName: pack.name,
          details: pack.metadata.description ? [pack.metadata.description] : [],
          iconUrl:
            folder === 'resourcepacks' && pack.metadata.icon ? resourcePackIconUrl(inst.id, pack.name) : undefined,
          fallbackIcon: folder === 'resourcepacks' ? 'image' : 'palette',
          managed: Boolean(entry),
          pack: membership.get(`${folder}/${base}`),
          enabled: pack.enabled,
          size: pack.size,
          update: update && {
            version: update.latest_version_number,
            disabled: busy,
            onClick: () => void applyModUpdates(inst, [update]),
          },
        },
      };
    })
    .sort((left, right) => compareContentRows(left.view, right.view));
  const shown = rows.filter(({ view }) => contentRowMatches(view, q));
  const selection = useSelection(
    shown.map(({ row }) => row),
    useCallback((row: PackRow) => row.pack.name, []),
  );
  const updates = rows.flatMap(({ row }) => (row.update ? [row.update] : []));
  const clearAndRefresh = (): void => {
    selection.clear();
    refresh();
  };

  return (
    <div class="cp-instance-body">
      <div class="cp-resource-toolbar">
        <div>
          <strong>
            {rows.length} {noun}
            {rows.length === 1 ? '' : 's'}
          </strong>
          <Input
            value={q}
            onChange={setQ}
            placeholder={`Filter ${noun}s…`}
            icon="search"
            style={{ width: 200, height: 30 }}
          />
        </div>
        <div>
          <Button variant="secondary" size="sm" icon="refresh" onClick={refresh}>
            Refresh
          </Button>
          <Button variant="secondary" size="sm" icon="folder" onClick={() => void openInstanceFolder(inst.id, folder)}>
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
      {updates.length > 0 && (
        <div class="cp-mods-updates" role="status">
          <Icon name="arrow-up" size={14} color="var(--accent)" />
          <span>
            {updates.length} update{updates.length === 1 ? '' : 's'} available
          </span>
          <span class="cp-mods-updates-action">
            <Button size="sm" disabled={busy} onClick={() => void applyModUpdates(inst, updates)}>
              Update all
            </Button>
          </span>
        </div>
      )}
      {state.status === 'error' && (
        <div class="cp-resource-note cp-resource-note--error" role="alert">
          <span>{state.error}</span>
          <Button variant="secondary" size="sm" icon="refresh" onClick={refresh}>
            Retry
          </Button>
        </div>
      )}
      <ResourceMutationStatus instanceId={inst.id} />
      <div class="cp-content-list">
        {state.status === 'loading' && rows.length === 0 ? (
          <div class="cp-mods-empty-row">Loading {noun}s…</div>
        ) : shown.length === 0 ? (
          <div class="cp-mods-empty-row">
            <strong>{rows.length === 0 ? `No ${noun}s in this instance` : `No ${noun}s match this filter`}</strong>
            {rows.length === 0 ? EMPTY_HINT[folder] : 'Try a different filter.'}
          </div>
        ) : (
          shown.map(({ row, view }) => (
            <ContentRow
              key={view.key}
              view={view}
              selected={selection.isSelected(row.pack.name)}
              onToggle={() => selection.toggle(row.pack.name)}
              onMenu={(e) =>
                openContextMenu(
                  e,
                  packMenuItems(inst, folder, row, refresh, selectionMenuItem(selection, row.pack.name)),
                )
              }
            />
          ))
        )}
      </div>
      <SelectionActionTray
        selection={selection}
        itemLabel={noun}
        actions={[
          {
            label: 'Remove',
            icon: 'trash',
            danger: true,
            disabled: busy,
            onClick: () => void removePacks(inst, folder, selection.selectedItems, clearAndRefresh, refresh),
          },
        ]}
      />
    </div>
  );
}
