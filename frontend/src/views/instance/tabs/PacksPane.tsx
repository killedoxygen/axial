import type { JSX } from 'preact';
import { useCallback, useEffect, useMemo, useState } from 'preact/hooks';
import { contentRevision } from '../../../content-activity';
import { Button, Empty, IconButton, Input } from '../../../ui/Atoms';
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
  UpdateAll,
  compareContentRows,
  contentRowMatches,
  fileDisplayName,
  type ContentRowView,
} from '../components/content-row';
import { packMembership } from '../components/pack-origin';
import { resourceMutationState } from '../bulk-actions';
import { applyModUpdates, cachedModProvenance, fetchModProvenance, type ModProvenance } from '../mod-actions';
import { PACK_NOUN, packMenuItems, removePacks, type PackRow } from '../pack-actions';
import { settleChildren } from '../../../ui/motion';

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
          pack: membership.get(`${folder}/${base}`),
          enabled: pack.enabled,
          folder: pack.directory,
          update: update && {
            version: update.latest_version_number,
            disabled: busy,
            onClick: () => void applyModUpdates(inst, [update]),
          },
        },
      };
    })
    .sort(
      (left, right) =>
        Number(Boolean(right.view.update)) - Number(Boolean(left.view.update)) ||
        compareContentRows(left.view, right.view),
    );
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

  const empty = state.status === 'ready' && rows.length === 0;
  const openFolder = (): void => void openInstanceFolder(inst.id, folder);

  return (
    <div class="cp-pane">
      {!empty && (
        <div class="cp-pane-bar">
          <Input value={q} onChange={setQ} placeholder={`Search ${noun}s`} icon="search" />
          {updates.length > 0 && (
            <UpdateAll
              count={updates.length}
              noun={noun}
              disabled={busy}
              onClick={() => void applyModUpdates(inst, updates)}
            />
          )}
          <span class="cp-head-fill" />
          <IconButton icon="refresh" tooltip="Refresh" onClick={refresh} />
          <IconButton icon="folder" tooltip="Open folder" onClick={openFolder} />
          <Button variant="soft" icon="plus" onClick={() => navigate({ name: 'discover', target: inst.id })}>
            Add {noun}s
          </Button>
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
      {state.status === 'loading' && rows.length === 0 ? (
        <div class="cp-resource-note">Loading {noun}s…</div>
      ) : empty ? (
        <Empty
          title={`No ${noun}s in this instance`}
          actions={
            <>
              <Button
                variant="secondary"
                icon="compass"
                onClick={() => navigate({ name: 'discover', target: inst.id })}
              >
                Browse Discover
              </Button>
              <Button variant="secondary" icon="folder" onClick={openFolder}>
                Open folder
              </Button>
            </>
          }
        >
          {EMPTY_HINT[folder]}
        </Empty>
      ) : shown.length === 0 ? (
        <Empty title={`No ${noun}s match this filter`}>Try a different filter.</Empty>
      ) : (
        <div class="cp-entries" data-settles ref={(list) => settleChildren(list)}>
          {shown.map(({ row, view }) => (
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
          ))}
        </div>
      )}
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
