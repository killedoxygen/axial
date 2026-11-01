import { Fragment, type JSX } from 'preact';
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

/** Names the modpack an instance was installed from, linking packs that have a Modrinth page. */
export function PackOrigin({
  packs,
  instanceId,
}: {
  packs: InstanceContentPack[];
  instanceId: string;
}): JSX.Element | null {
  if (packs.length === 0) return null;
  return (
    <div class="cp-pack-origin">
      <Icon name="stack" size={14} />
      <span>
        Installed from{' '}
        {packs.map((pack, index) => (
          <Fragment key={pack.canonical_id}>
            {index > 0 && ', '}
            {pack.provider === 'local' ? (
              packLabel(pack)
            ) : (
              <button
                type="button"
                class="cp-pack-origin-link"
                onClick={() => navigate({ name: 'content', id: pack.canonical_id, target: instanceId })}
              >
                {packLabel(pack)}
              </button>
            )}
          </Fragment>
        ))}
      </span>
    </div>
  );
}
