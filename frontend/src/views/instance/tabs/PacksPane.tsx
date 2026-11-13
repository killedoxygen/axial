import type { JSX } from 'preact';
import { useMemo, useState } from 'preact/hooks';
import { mediaTicketEpoch } from '../../../api';
import { Button } from '../../../ui/Atoms';
import { navigate } from '../../../ui-state';
import type { EnrichedInstance } from '../../../types-instance';
import { fetchPacks, fileArtUrl, type PackFolder } from '../resources';
import { useListing, useModProvenance } from '../listings';
import { openInstanceFolder } from '../instance-actions';
import { EntryPane, compareEntries, fileDisplayName, type EntryView } from '../components/entries';
import { packMembership } from '../components/pack-origin';
import { resourceMutationState } from '../bulk-actions';
import { applyModUpdates, updateFor } from '../mod-actions';
import { PACK_NOUN, packMenuItems, removePacks, type PackRow } from '../pack-actions';
import { updateLabel } from '../version-label';

const KIND = { resourcepacks: 'resource_pack', shaderpacks: 'shader_pack' } as const;

const EMPTY_HINT: Record<PackFolder, string> = {
  resourcepacks:
    'Find resource packs in Discover, or drop .zip files into the folder. Turn them on in game from Options.',
  shaderpacks:
    'Find shaders in Discover, or drop .zip files into the folder. Shaders need a shader loader such as Iris.',
};

export function PacksPane({ inst, folder }: { inst: EnrichedInstance; folder: PackFolder }): JSX.Element {
  const noun = PACK_NOUN[folder];
  const [stamp, setStamp] = useState(0);
  const listing = useListing(inst.id, folder, () => fetchPacks(inst.id, folder));
  const { provenance } = useModProvenance(inst.id, stamp);
  const busy = resourceMutationState(inst.id).status === 'pending';

  const refresh = (): void => {
    setStamp((value) => value + 1);
    listing.refresh();
  };

  const game = inst.version_display.minecraft_label;
  const { rows, byKey } = useMemo(() => {
    const byKey = new Map<string, PackRow>();
    const membership = packMembership(provenance?.packs ?? []);
    const managed = new Map(
      (provenance?.managed ?? [])
        .filter((entry) => entry.kind === KIND[folder])
        .map((entry) => [entry.filename, entry]),
    );
    const rows = (listing.data ?? []).map((pack): EntryView => {
      const base = pack.name.replace(/\.disabled$/i, '');
      const entry = pack.directory ? undefined : managed.get(base);
      const update = updateFor(provenance, entry);
      byKey.set(pack.name, { pack, entry, update });
      return {
        key: pack.name,
        title: entry?.title ?? fileDisplayName(pack.name),
        fileName: pack.name,
        byline: pack.metadata.description,
        artUrl:
          folder === 'resourcepacks' && pack.metadata.icon
            ? fileArtUrl(inst.id, folder, pack.name, pack.modified_at)
            : undefined,
        icon: folder === 'resourcepacks' ? 'image' : 'palette',
        pack: membership.get(`${folder}/${base}`),
        disabled: !pack.enabled,
        folder: pack.directory,
        update: update && updateLabel(update.latest_version_number, game, update.latest_channel),
      };
    });
    return { rows: rows.sort(compareEntries), byKey };
  }, [listing.data, provenance, folder, game, inst.id, mediaTicketEpoch]);

  const pending = useMemo(() => [...byKey.values()].flatMap((row) => (row.update ? [row.update] : [])), [byKey]);
  const openFolder = (): void => void openInstanceFolder(inst.id, folder);
  const browse = (): void => navigate({ name: 'discover', target: inst.id });

  return (
    <EntryPane
      instanceId={inst.id}
      noun={noun}
      rows={rows}
      state={listing}
      expected={
        inst.counts_available === false
          ? undefined
          : folder === 'resourcepacks'
            ? inst.resource_count
            : inst.shader_count
      }
      onRefresh={refresh}
      onOpenFolder={openFolder}
      updates={{
        count: pending.length,
        all: () => void applyModUpdates(inst, pending),
        one: (key) => {
          const update = byKey.get(key)?.update;
          if (update) void applyModUpdates(inst, [update]);
        },
      }}
      tools={
        <Button variant="soft" icon="plus" onClick={browse}>
          Add {noun}s
        </Button>
      }
      empty={{
        title: `No ${noun}s in this instance`,
        hint: EMPTY_HINT[folder],
        actions: (
          <>
            <Button variant="secondary" icon="compass" onClick={browse}>
              Browse Discover
            </Button>
            <Button variant="secondary" icon="folder" onClick={openFolder}>
              Open folder
            </Button>
          </>
        ),
      }}
      menu={(key, selectionItem) => {
        const row = byKey.get(key);
        return row ? packMenuItems(inst, folder, row, refresh, selectionItem) : [];
      }}
      actions={(selected, clear) => [
        {
          label: 'Remove',
          icon: 'trash',
          danger: true,
          disabled: busy,
          onClick: () =>
            void removePacks(
              inst,
              folder,
              selected.flatMap((view) => byKey.get(view.key) ?? []),
              () => {
                clear();
                refresh();
              },
              refresh,
            ),
        },
      ]}
    />
  );
}
