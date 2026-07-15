import { api } from '../../api';
import { checkContentUpdates, installContent, listInstanceContent, uninstallContents } from '../../content';
import { applyInstallQueueResponse } from '../../machines/downloads';
import { toast } from '../../toast';
import { navigate } from '../../ui-state';
import { errMessage, modBaseName } from '../../utils';
import type { ContextMenuItem } from '../../ui/ContextMenu';
import type { ContentUpdate, InstanceContentEntry } from '../../types-content';
import type { EnrichedInstance, InstanceMod } from '../../types-instance';
import { openInstanceFolder } from './instance-actions';
import { confirmDeleteItems, partialFailureMessage, runBulkMutation } from './bulk-actions';
import {
  beginModProvenanceRefresh,
  cacheModProvenance,
  cachedModProvenance,
  isCurrentModProvenanceRefresh,
  type ModProvenance,
} from './mod-provenance-cache';
const CONTENT_INSTALL_BATCH_LIMIT = 40;

export { cachedModProvenance, type ModProvenance } from './mod-provenance-cache';

/** Provenance and update state for an instance's mods, keyed by filename and
 * canonical id. Names land as soon as the listing does; the update check is
 * best-effort and streams in as a second snapshot so it never delays them.
 * Snapshots are cached per instance so revisiting the tab paints instantly. */
export async function fetchModProvenance(
  instanceId: string,
  onData: (provenance: ModProvenance) => void,
): Promise<void> {
  const generation = beginModProvenanceRefresh(instanceId);
  const cached = cachedModProvenance(instanceId);
  if (cached) {
    const refreshing: ModProvenance = { entries: cached.entries, updates: new Map() };
    cacheModProvenance(instanceId, refreshing);
    onData(refreshing);
  }
  const content = await listInstanceContent(instanceId);
  if (!isCurrentModProvenanceRefresh(instanceId, generation)) return;
  const entries = new Map(
    content.entries.filter((entry) => entry.kind === 'mod').map((entry) => [entry.filename, entry]),
  );
  const listed: ModProvenance = { entries, updates: new Map() };
  cacheModProvenance(instanceId, listed);
  onData(listed);
  try {
    const res = await checkContentUpdates(instanceId);
    if (!isCurrentModProvenanceRefresh(instanceId, generation)) return;
    const updates = new Map<string, ContentUpdate>();
    for (const update of res.updates) {
      if (update.kind === 'mod') updates.set(update.canonical_id, update);
    }
    const checked: ModProvenance = { entries, updates };
    cacheModProvenance(instanceId, checked);
    onData(checked);
  } catch {
    // A failed check reads as "no updates"; the list itself still works offline.
  }
}

export async function applyModUpdates(
  inst: EnrichedInstance,
  updates: ContentUpdate[],
  onDone: () => void,
): Promise<void> {
  if (updates.length === 0) return;
  const single = updates.length === 1 ? (updates[0].title ?? 'mod') : null;
  const label = single ? `Updating ${single}` : `Updating ${updates.length} mods`;
  toast(`${label}…`, 'info');
  let queuedCount = 0;
  try {
    for (let offset = 0; offset < updates.length; offset += CONTENT_INSTALL_BATCH_LIMIT) {
      const batch = updates.slice(offset, offset + CONTENT_INSTALL_BATCH_LIMIT);
      const queue = await installContent(
        inst.id,
        batch.map((update) => ({
          canonical_id: update.canonical_id,
          kind: update.kind,
          version_id: update.latest_version_id,
        })),
      );
      queuedCount += batch.length;
      const finalBatch = queuedCount === updates.length;
      await applyInstallQueueResponse(queue, {
        showNotice: finalBatch,
        connectActive: finalBatch,
      });
    }
    toast(single ? `${single} update queued` : `${updates.length} mod updates queued`);
  } catch (err) {
    const prefix = queuedCount > 0 ? `Queued ${queuedCount} of ${updates.length} updates. ` : '';
    toast(`${prefix}Could not queue the remaining updates: ${errMessage(err)}`, 'error');
  }
}

export async function removeManagedMod(
  inst: EnrichedInstance,
  entry: InstanceContentEntry,
  onDone: () => void,
): Promise<void> {
  const confirmed = await confirmDeleteItems({
    count: 1,
    itemLabel: 'mod',
    message: `Remove "${entry.title ?? entry.filename}" from this instance. This deletes the file and its install record.`,
  });
  if (!confirmed) return;
  try {
    await queueManagedModRemoval(inst, entry, true);
    toast('Mod removal queued');
  } catch (err) {
    toast(`Could not remove the mod: ${errMessage(err)}`, 'error');
  }
}

async function queueManagedModRemoval(
  inst: EnrichedInstance,
  entry: InstanceContentEntry,
  showNotice: boolean,
): Promise<void> {
  await queueManagedModRemovals(inst, [entry], showNotice);
}

async function queueManagedModRemovals(
  inst: EnrichedInstance,
  entries: InstanceContentEntry[],
  showNotice: boolean,
): Promise<void> {
  const queue = await uninstallContents(
    inst.id,
    entries.map((entry) => entry.canonical_id),
  );
  await applyInstallQueueResponse(queue, { showNotice, connectActive: true });
}

async function updateModEnabled(inst: EnrichedInstance, modName: string, enabled: boolean): Promise<void> {
  const res: any = await api('PUT', `/instances/${encodeURIComponent(inst.id)}/mods/${encodeURIComponent(modName)}`, {
    enabled,
  });
  if (res?.error) throw new Error(res.error);
}

async function removeMod(inst: EnrichedInstance, modName: string): Promise<void> {
  const res: any = await api('DELETE', `/instances/${encodeURIComponent(inst.id)}/mods/${encodeURIComponent(modName)}`);
  if (res?.error) throw new Error(res.error);
}

export async function setModEnabled(inst: EnrichedInstance, mod: InstanceMod, onDone: () => void): Promise<void> {
  try {
    await updateModEnabled(inst, mod.name, !mod.enabled);
    toast(!mod.enabled ? 'Mod enabled' : 'Mod disabled');
    onDone();
  } catch (err) {
    toast(`Could not update the mod: ${errMessage(err)}`, 'error');
  }
}

export async function setModsEnabled(
  inst: EnrichedInstance,
  mods: InstanceMod[],
  enabled: boolean,
  onDone: () => void,
): Promise<void> {
  const changed = mods.filter((mod) => mod.enabled !== enabled);
  if (changed.length === 0) {
    toast(enabled ? 'Selected mods are already enabled' : 'Selected mods are already disabled', 'info');
    return;
  }
  await runBulkMutation({
    items: changed,
    action: (mod) => updateModEnabled(inst, mod.name, enabled),
    success: (count) => (enabled ? `${count} mods enabled` : `${count} mods disabled`),
    partial: (done, total, err) => partialFailureMessage('Updated', done, total, err),
    onDone,
  });
}

export async function deleteMods(
  inst: EnrichedInstance,
  mods: InstanceMod[],
  onDone: () => void,
  managedEntries: ReadonlyMap<string, InstanceContentEntry> = new Map(),
): Promise<void> {
  const removals = mods.map((mod) => ({
    mod,
    entry: managedEntries.get(modBaseName(mod.name)),
  }));
  const managedCount = removals.filter(({ entry }) => entry !== undefined).length;
  const confirmed = await confirmDeleteItems({
    count: mods.length,
    itemLabel: 'mod',
    message:
      mods.length === 1
        ? `Remove "${mods[0]!.name}" from this instance. This deletes the mod file${managedCount ? ' and its install record' : ''}.`
        : `Remove ${mods.length} mods from this instance. Managed mods are safely removed from the install record too.`,
  });
  if (!confirmed) return;
  const managed = removals.flatMap(({ entry }) => (entry ? [entry] : []));
  const unmanaged = removals.flatMap(({ mod, entry }) => (entry ? [] : [mod]));
  let started = 0;
  try {
    if (managed.length > 0) {
      await queueManagedModRemovals(inst, managed, false);
      started += managed.length;
    }
    for (const mod of unmanaged) {
      await removeMod(inst, mod.name);
      started += 1;
    }
    toast(started === 1 ? 'Mod removal started' : `${started} mod removals started`);
  } catch (err) {
    toast(partialFailureMessage('Started removal for', started, mods.length, err), 'error');
  }
  onDone();
}

export function modMenuItems(
  inst: EnrichedInstance,
  mod: InstanceMod,
  onRefresh: () => void,
  selectionItem: ContextMenuItem,
  provenance?: { entry?: InstanceContentEntry; update?: ContentUpdate },
): ContextMenuItem[] {
  const entry = provenance?.entry;
  const update = provenance?.update;
  return [
    selectionItem,
    { divider: true, label: '', onSelect: () => undefined },
    ...(update
      ? [
          {
            icon: 'arrow-up',
            label: `Update to ${update.latest_version_number}`,
            onSelect: () => void applyModUpdates(inst, [update], onRefresh),
          },
        ]
      : []),
    {
      icon: mod.enabled ? 'stop' : 'play',
      label: mod.enabled ? 'Disable' : 'Enable',
      onSelect: () => void setModEnabled(inst, mod, onRefresh),
    },
    ...(entry
      ? [
          {
            icon: 'compass',
            label: 'View in Discover',
            onSelect: () => navigate({ name: 'content', id: entry.canonical_id, target: inst.id }),
          },
        ]
      : []),
    { icon: 'folder', label: 'Open mods folder', onSelect: () => void openInstanceFolder(inst.id, 'mods') },
    { icon: 'refresh', label: 'Refresh list', onSelect: onRefresh },
    { divider: true, label: '', onSelect: () => undefined },
    entry
      ? { icon: 'trash', label: 'Remove', onSelect: () => void removeManagedMod(inst, entry, onRefresh), danger: true }
      : { icon: 'trash', label: 'Delete', onSelect: () => void deleteMods(inst, [mod], onRefresh), danger: true },
  ];
}
