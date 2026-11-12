import { plural } from '../../format';
import { createInstance } from '../../instance-create';
import { config } from '../../store';
import type { ModpackTarget } from '../../types-content';
import { navigate, openCreateDraft, openCreateModpack } from '../../ui-state';
import { defaultIconFor, type LoaderKey } from '../create/defaults';
import { commitInstall } from './actions';
import type { Fit } from './fit';
import { selectionOf, unstageAll, type TrayItem } from './state';

export interface StagedPack {
  item: TrayItem;
  target: ModpackTarget | undefined;
  fit: Fit | null;
}

/**
 * Queue the picks a pack's instance can take behind the pack, so they install once it has. Resolves the ones that
 * were not queued; they stay picked.
 */
export async function addPicksTo(
  instanceId: string,
  extras: TrayItem[],
  drops: ReadonlySet<string>,
): Promise<TrayItem[]> {
  const kept = extras.filter((item) => !drops.has(item.canonical_id));
  if (kept.length === 0) return [];
  const outcome = await commitInstall(instanceId, kept.map(selectionOf), plural(kept.length, 'pick', 'picks'));
  return outcome.status === 'queued' ? [] : kept;
}

/** The create form for what is staged: a pack with the other picks, or the picks alone on a version that fits them. */
export function openStaged(pack: StagedPack | undefined, extras: TrayItem[]): void {
  if (!pack) openCreateDraft(extras);
  else if (pack.target)
    openCreateModpack({ ...pack.target, icon_url: pack.item.icon_url, pick: selectionOf(pack.item) }, extras);
}

/** Several packs: an instance each on the launcher's defaults, with the other picks added where they fit. */
export async function createEach(packs: StagedPack[], extras: TrayItem[]): Promise<void> {
  const made: TrayItem[] = [];
  const held = new Set<string>();
  for (const { item, target, fit } of packs) {
    // A pack whose picks could not be checked stays staged: what its instance takes is not known.
    if (!target || !fit?.drops) continue;
    const result = await createInstance({
      name: target.name,
      selectionId: target.selection_id,
      icon: defaultIconFor((target.loader ?? 'vanilla') as LoaderKey),
      accent: config.value?.theme ?? '',
      modpack: { canonicalId: target.canonical_id, versionId: target.version_id },
      stay: true,
    });
    if (!result.ok || !result.instance) continue;
    made.push(item);
    for (const pick of await addPicksTo(result.instance.id, extras, fit.drops)) {
      held.add(pick.canonical_id);
    }
  }
  if (made.length === 0) return;
  // Picks stay while a pack is still to be made, and the ones an instance did not take stay to be added again.
  const all = made.length === packs.length;
  const taken = all ? extras.filter((extra) => !held.has(extra.canonical_id)) : [];
  unstageAll([...made, ...taken].map(selectionOf));
  if (all && held.size === 0) navigate({ name: 'instances' });
}
