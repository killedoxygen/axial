import type { JSX } from 'preact';
import { Chip } from '../../../ui/Atoms';
import { Icon } from '../../../ui/Icons';
import { navigate } from '../../../ui-state';
import type { InstanceContentPack } from '../../../types-content';

/** Instance paths (`mods/x.jar`) mapped to the name of the pack that placed them. */
export function packMembership(packs: InstanceContentPack[]): Map<string, string> {
  const members = new Map<string, string>();
  for (const pack of packs) {
    for (const path of pack.paths) members.set(path, pack.title ?? 'modpack');
  }
  return members;
}

export function packLabel(pack: InstanceContentPack): string {
  const title = pack.title ?? 'a modpack';
  return pack.version_number ? `${title} ${pack.version_number}` : title;
}

/** The modpacks an instance was installed from, as chips; a pack with a Modrinth page opens it. */
export function PackChips({ packs, instanceId }: { packs: InstanceContentPack[]; instanceId: string }): JSX.Element {
  return (
    <>
      {packs.map((pack) =>
        pack.provider === 'local' ? (
          <Chip key={pack.canonical_id} icon="stack" title={`Installed from ${packLabel(pack)}`}>
            {packLabel(pack)}
          </Chip>
        ) : (
          <button
            key={pack.canonical_id}
            type="button"
            class="cp-chip cp-pack-chip"
            title={`Installed from ${packLabel(pack)}`}
            onClick={() => navigate({ name: 'content', id: pack.canonical_id, target: instanceId })}
          >
            <Icon name="stack" size={11} />
            {packLabel(pack)}
          </button>
        ),
      )}
    </>
  );
}
