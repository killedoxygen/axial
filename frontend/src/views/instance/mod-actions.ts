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
import { confirmDeleteItems, partialFailureMessage, runBulkMutation, runResourceMutation } from './bulk-actions';
import {
  beginModProvenanceRefresh,
  cacheModProvenance,
  cachedModProvenance,
  isCurrentModProvenanceRefresh,
  type ModProvenance,
} from './mod-provenance-cache';
import { requireResourceCommandSuccess } from './resources';
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
  } catch (err) {
    if (!isCurrentModProvenanceRefresh(instanceId, generation)) return;
    const failed: ModProvenance = { ...listed, updateError: `Could not check mod updates: ${errMessage(err)}` };
    cacheModProvenance(instanceId, failed);
    onData(failed);
  }
}

export async function applyModUpdates(inst: EnrichedInstance, updates: ContentUpdate[]): Promise<void> {
  if (updates.length === 0) return;
  await runResourceMutation(inst.id, 'Queue mod updates', async () => {
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
      throw new Error(`${prefix}Could not confirm the remaining update requests: ${errMessage(err)}`);
    }
  });
}

export async function removeManagedMod(inst: EnrichedInstance, entry: InstanceContentEntry): Promise<void> {
  await runResourceMutation(inst.id, 'Remove mod', async () => {
    const confirmed = await confirmDeleteItems({
      count: 1,
      itemLabel: 'mod',
      message: `Remove "${entry.title ?? entry.filename}" from this instance. This deletes the file and its install record.`,
    });
    if (!confirmed) return;
    await queueManagedModRemoval(inst, entry, true);
    toast('Mod removal queued');
  });
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
  const res = await api('PUT', `/instances/${encodeURIComponent(inst.id)}/mods/${encodeURIComponent(modName)}`, {
    enabled,
  });
  requireResourceCommandSuccess(res, 'Mod update');
}

async function removeMod(inst: EnrichedInstance, modName: string): Promise<void> {
  requireResourceCommandSuccess(
    await api('DELETE', `/instances/${encodeURIComponent(inst.id)}/mods/${encodeURIComponent(modName)}`),
    'Mod deletion',
  );
}

export async function setModEnabled(inst: EnrichedInstance, mod: InstanceMod, onDone: () => void): Promise<void> {
  await runResourceMutation(inst.id, mod.enabled ? 'Disable mod' : 'Enable mod', async () => {
    await updateModEnabled(inst, mod.name, !mod.enabled);
    toast(!mod.enabled ? 'Mod enabled' : 'Mod disabled');
    onDone();
  });
}

export async function setModsEnabled(
  inst: EnrichedInstance,
  mods: InstanceMod[],
  enabled: boolean,
  onDone: () => void,
  onFailure?: () => void,
): Promise<void> {
  const changed = mods.filter((mod) => mod.enabled !== enabled);
  if (changed.length === 0) {
    toast(enabled ? 'Selected mods are already enabled' : 'Selected mods are already disabled', 'info');
    return;
  }
  await runResourceMutation(inst.id, enabled ? 'Enable mods' : 'Disable mods', () =>
    runBulkMutation({
      items: changed,
      action: (mod) => updateModEnabled(inst, mod.name, enabled),
      success: (count) => (enabled ? `${count} mods enabled` : `${count} mods disabled`),
      partial: (done, total, err) => partialFailureMessage('Updated', done, total, err),
      onDone,
      onFailure,
    }),
  );
}

export async function deleteMods(
  inst: EnrichedInstance,
  mods: InstanceMod[],
  onDone: () => void,
  managedEntries: ReadonlyMap<string, InstanceContentEntry> = new Map(),
  onFailure?: () => void,
): Promise<void> {
  if (mods.length === 0) return;
  await runResourceMutation(inst.id, 'Delete mods', async () => {
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
      onFailure?.();
      throw new Error(partialFailureMessage('Started removal for', started, mods.length, err));
    }
    onDone();
  });
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
  const items: ContextMenuItem[] = [selectionItem, { divider: true, label: '', onSelect: () => undefined }];
  if (update) {
    items.push({
      icon: 'arrow-up',
      label: `Update to ${update.latest_version_number}`,
      onSelect: () => void applyModUpdates(inst, [update]),
    });
  }
  items.push({
    icon: mod.enabled ? 'stop' : 'play',
    label: mod.enabled ? 'Disable' : 'Enable',
    onSelect: () => void setModEnabled(inst, mod, onRefresh),
  });
  if (entry) {
    items.push({
      icon: 'compass',
      label: 'View in Discover',
      onSelect: () => navigate({ name: 'content', id: entry.canonical_id, target: inst.id }),
    });
  }
  items.push(
    { icon: 'folder', label: 'Open mods folder', onSelect: () => void openInstanceFolder(inst.id, 'mods') },
    { icon: 'refresh', label: 'Refresh list', onSelect: onRefresh },
    { divider: true, label: '', onSelect: () => undefined },
    entry
      ? { icon: 'trash', label: 'Remove', onSelect: () => void removeManagedMod(inst, entry), danger: true }
      : {
          icon: 'trash',
          label: 'Delete',
          onSelect: () => void deleteMods(inst, [mod], onRefresh, undefined, onRefresh),
          danger: true,
        },
  );
  return items;
}
