import { api } from '../../api';
import { checkContentUpdates, installContent, listInstanceContent, uninstallContents } from '../../content';
import { requiredRefusal } from '../../dto-content';
import { applyInstallQueueResponse } from '../../machines/downloads';
import { toast } from '../../toast';
import { showChoice, showConfirm } from '../../ui/Dialog';
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
    const refreshing: ModProvenance = { ...cached, updates: new Map(), updateError: undefined };
    cacheModProvenance(instanceId, refreshing);
    onData(refreshing);
  }
  const content = await listInstanceContent(instanceId);
  if (!isCurrentModProvenanceRefresh(instanceId, generation)) return;
  const entries = new Map(
    content.entries.filter((entry) => entry.kind === 'mod').map((entry) => [entry.filename, entry]),
  );
  const listed: ModProvenance = { entries, managed: content.entries, packs: content.packs ?? [], updates: new Map() };
  cacheModProvenance(instanceId, listed);
  onData(listed);
  try {
    const res = await checkContentUpdates(instanceId);
    if (!isCurrentModProvenanceRefresh(instanceId, generation)) return;
    const updates = new Map<string, ContentUpdate>();
    for (const update of res.updates) {
      updates.set(update.canonical_id, update);
    }
    const checked: ModProvenance = { ...listed, updates };
    cacheModProvenance(instanceId, checked);
    onData(checked);
  } catch (err) {
    if (!isCurrentModProvenanceRefresh(instanceId, generation)) return;
    const failed: ModProvenance = { ...listed, updateError: `Could not check mod updates: ${errMessage(err)}` };
    cacheModProvenance(instanceId, failed);
    onData(failed);
  }
}

/** Updates "Update all" applies. Modpack members stay coupled to their pack:
 * a newer member can need a newer loader or pack-mate than the pack ships. */
export function bulkModUpdates(updates: ContentUpdate[]): ContentUpdate[] {
  return updates.filter((update) => !update.pack);
}

export async function applyModUpdates(inst: EnrichedInstance, requested: ContentUpdate[]): Promise<void> {
  const updates = requested.length === 1 ? requested : bulkModUpdates(requested);
  if (updates.length === 0) return;
  const member = updates.length === 1 ? updates[0]!.pack : undefined;
  if (member) {
    const name = updates[0]!.title ?? 'This mod';
    const confirmed = await showConfirm(
      "Updating it on its own may break the pack, and the pack's next update may replace it.",
      { title: `${name} came with ${member.title ?? 'this modpack'}`, confirmText: 'Update anyway' },
    );
    if (!confirmed) return;
  }
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

function contentName(entry: { title?: string; filename?: string; canonical_id: string }): string {
  return entry.title ?? entry.filename ?? entry.canonical_id;
}

function names(values: string[]): string {
  if (values.length <= 1) return values[0] ?? '';
  if (values.length > 3) return `${values.slice(0, 3).join(', ')} and ${values.length - 3} more`;
  return `${values.slice(0, -1).join(', ')} and ${values[values.length - 1]}`;
}

/** Enabled content outside `removing` that needs it, from a fresh listing. */
async function dependentsOf(inst: EnrichedInstance, removing: InstanceContentEntry[]): Promise<InstanceContentEntry[]> {
  const listed = (await listInstanceContent(inst.id)).entries;
  const ids = new Set(removing.map((entry) => entry.canonical_id));
  const needed = new Set(
    listed.filter((entry) => ids.has(entry.canonical_id)).flatMap((entry) => entry.required_by ?? []),
  );
  return listed.filter((entry) => needed.has(entry.canonical_id) && !ids.has(entry.canonical_id));
}

/** Ask to remove the content that needs `removing` along with it. Resolves to
 * the extra entries to remove, or null when the person keeps everything. */
async function confirmDependentRemoval(
  removing: InstanceContentEntry[],
  dependents: InstanceContentEntry[],
): Promise<InstanceContentEntry[] | null> {
  const target = removing.length === 1 ? contentName(removing[0]!) : `these ${removing.length} mods`;
  const total = removing.length + dependents.length;
  const choice = await showChoice<'all'>(
    `${names(dependents.map(contentName))} ${dependents.length === 1 ? 'depends' : 'depend'} on it and will stop working.`,
    [{ value: 'all', label: total === 2 ? 'Remove both' : `Remove all ${total}`, variant: 'danger' }],
    {
      title: `Other mods need ${target}`,
      cancelText: removing.length === 1 ? `Keep ${contentName(removing[0]!)}` : 'Keep them',
    },
  );
  return choice === 'all' ? dependents : null;
}

export async function removeManagedMod(inst: EnrichedInstance, entry: InstanceContentEntry): Promise<void> {
  await runResourceMutation(inst.id, 'Remove mod', async () => {
    const dependents = await dependentsOf(inst, [entry]);
    let removing = [entry];
    if (dependents.length > 0) {
      const extra = await confirmDependentRemoval(removing, dependents);
      if (!extra) return;
      removing = [...removing, ...extra];
    } else {
      const confirmed = await confirmDeleteItems({
        count: 1,
        itemLabel: 'mod',
        message: `Remove "${entry.title ?? entry.filename}" from this instance. This deletes the file and its install record.`,
      });
      if (!confirmed) return;
    }
    await queueManagedModRemovals(inst, removing, true);
    toast(removing.length === 1 ? 'Mod removal queued' : `${removing.length} mod removals queued`);
  });
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

async function updateModEnabled(
  inst: EnrichedInstance,
  modName: string,
  enabled: boolean,
  withDependents = false,
): Promise<void> {
  const res = await api('PUT', `/instances/${encodeURIComponent(inst.id)}/mods/${encodeURIComponent(modName)}`, {
    enabled,
    ...(withDependents ? { with_dependents: true } : {}),
  });
  requireResourceCommandSuccess(res, 'Mod update');
}

/** Disable a mod; when others need it, offer to disable them too. Resolves
 * false when the person keeps it enabled. */
async function disableMod(inst: EnrichedInstance, mod: InstanceMod, title: string): Promise<boolean> {
  try {
    await updateModEnabled(inst, mod.name, false);
    return true;
  } catch (err) {
    const refusal = requiredRefusal(err);
    if (!refusal) throw err;
    const dependents = refusal.required_by.map(contentName);
    const total = dependents.length + 1;
    const choice = await showChoice<'all'>(
      `Disabling ${title} will break ${dependents.length === 1 ? dependents[0] : `${dependents.length} other mods`}. ${
        total === 2 ? 'Disable both?' : `Disable all ${total}?`
      }`,
      [{ value: 'all', label: total === 2 ? 'Disable both' : `Disable all ${total}`, variant: 'primary' }],
      { title: refusal.error.replace(/\.$/, '') },
    );
    if (choice !== 'all') return false;
    await updateModEnabled(inst, mod.name, false, true);
    return true;
  }
}

async function removeMod(inst: EnrichedInstance, modName: string): Promise<void> {
  requireResourceCommandSuccess(
    await api('DELETE', `/instances/${encodeURIComponent(inst.id)}/mods/${encodeURIComponent(modName)}`),
    'Mod deletion',
  );
}

export async function setModEnabled(
  inst: EnrichedInstance,
  mod: InstanceMod,
  onDone: () => void,
  title = mod.name,
): Promise<void> {
  await runResourceMutation(inst.id, mod.enabled ? 'Disable mod' : 'Enable mod', async () => {
    if (mod.enabled) {
      if (!(await disableMod(inst, mod, title))) return;
    } else {
      await updateModEnabled(inst, mod.name, true);
    }
    toast(!mod.enabled ? 'Mod enabled' : 'Mod disabled');
    onDone();
  });
}

/** Disable dependents before what they need: a dependent's own dependents are
 * a strict subset of its dependency's, so fewer dependents goes first. */
async function disableOrder(inst: EnrichedInstance, mods: InstanceMod[]): Promise<InstanceMod[]> {
  if (mods.length < 2) return mods;
  const listed = await listInstanceContent(inst.id).catch(() => null);
  if (!listed) return mods;
  const dependents = new Map(listed.entries.map((entry) => [entry.filename, entry.required_by?.length ?? 0]));
  const count = (mod: InstanceMod): number => dependents.get(modBaseName(mod.name)) ?? 0;
  return [...mods].sort((left, right) => count(left) - count(right));
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
  const ordered = enabled ? changed : await disableOrder(inst, changed);
  await runResourceMutation(inst.id, enabled ? 'Enable mods' : 'Disable mods', () =>
    runBulkMutation({
      items: ordered,
      action: (mod) => updateModEnabled(inst, mod.name, enabled),
      success: (count) => (enabled ? `${count} mods enabled` : `${count} mods disabled`),
      partial: (done, total, err) => partialFailureMessage('Update', done, total, err),
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
    let managed = removals.flatMap(({ entry }) => (entry ? [entry] : []));
    const dependents = managed.length > 0 ? await dependentsOf(inst, managed) : [];
    if (dependents.length > 0) {
      const extra = await confirmDependentRemoval(managed, dependents);
      if (!extra) return;
      managed = [...managed, ...extra];
    } else {
      const confirmed = await confirmDeleteItems({
        count: mods.length,
        itemLabel: 'mod',
        message:
          mods.length === 1
            ? `Remove "${mods[0]!.name}" from this instance. This deletes the mod file${managedCount ? ' and its install record' : ''}.`
            : `Remove ${mods.length} mods from this instance. Managed mods are safely removed from the install record too.`,
      });
      if (!confirmed) return;
    }
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
      throw new Error(partialFailureMessage('Removal request', started, managed.length + unmanaged.length, err));
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
    onSelect: () => void setModEnabled(inst, mod, onRefresh, entry?.title),
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
