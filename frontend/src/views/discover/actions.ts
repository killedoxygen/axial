import { contentCompatibility, getModpackTarget, installContent, installModpack, planContent } from '../../content';
import { createInstance } from '../../instance-create';
import { toast } from '../../toast';
import { errMessage } from '../../utils';
import { navigate } from '../../ui-state';
import { defaultIconFor, type LoaderKey } from '../create/defaults';
import type { CompatCandidate, ContentSelection, ResolutionPlan } from '../../types-content';
import { clearTray, markInstalled } from './state';
import { plural } from './shared';

/**
 * Adding content is silent when there is nothing to decide: the plan resolves,
 * the files land, a toast says so. A confirmation only interrupts when the plan
 * found a conflict, which is the one case where the user's answer changes what
 * happens.
 */
export interface AddOutcome {
  status: 'installed' | 'needs-confirmation' | 'failed';
  plan?: ResolutionPlan;
  error?: string;
}

export async function addToInstance(
  instanceId: string,
  selections: ContentSelection[],
  label: string,
): Promise<AddOutcome> {
  let plan: ResolutionPlan;
  try {
    plan = await planContent({ kind: 'instance', instance_id: instanceId }, selections);
  } catch (error) {
    const message = errMessage(error);
    toast(message, 'error');
    return { status: 'failed', error: message };
  }

  if (plan.conflicts.length > 0) {
    return { status: 'needs-confirmation', plan };
  }

  return commitInstall(instanceId, selections, label, plan);
}

export async function commitInstall(
  instanceId: string,
  selections: ContentSelection[],
  label: string,
  plan?: ResolutionPlan,
): Promise<AddOutcome> {
  try {
    await installContent(instanceId, selections);
  } catch (error) {
    const message = errMessage(error);
    toast(message, 'error');
    return { status: 'failed', error: message };
  }

  markInstalled(selections.map((selection) => selection.canonical_id));

  const extra = plan ? plan.items.filter((item) => item.reason === 'dependency' && !item.already_installed).length : 0;
  const suffix = extra > 0 ? ` with ${plural(extra, 'dependency', 'dependencies')}` : '';
  toast(`Added ${label}${suffix}`, 'success');
  return { status: 'installed', plan };
}

export function candidatesFor(selections: ContentSelection[]): Promise<CompatCandidate[]> {
  return contentCompatibility(selections).then((response) => response.candidates);
}

/**
 * Build the instance a staged set implies, then fill it. The create API is
 * addressable by loader and Minecraft version, so the candidate the backend
 * ranked is enough to make the instance — no version browsing needed.
 */
export async function createFromDraft(
  candidate: CompatCandidate,
  selections: ContentSelection[],
  name: string,
): Promise<boolean> {
  const dropped = new Set(candidate.drops.map((drop) => drop.canonical_id));
  const kept = selections.filter((selection) => !dropped.has(selection.canonical_id));
  if (kept.length === 0) return false;

  const loader = (candidate.loader || 'vanilla') as LoaderKey;
  const created = await createInstance({
    name,
    selectionId: candidate.selection_id,
    icon: defaultIconFor(loader),
    accent: '',
  });
  if (!created.ok || !created.instance) return false;

  const instanceId = created.instance.id;
  try {
    await installContent(instanceId, kept);
  } catch (error) {
    // The instance is real and already downloading; only the content failed.
    toast(`Created ${name}, but the content failed: ${errMessage(error)}`, 'error');
    return false;
  }

  clearTray();
  const droppedNote = dropped.size > 0 ? `, without ${plural(dropped.size, 'item', 'items')} that do not fit` : '';
  toast(`Created ${name} with ${plural(kept.length, 'item', 'items')}${droppedNote}`, 'success');
  return true;
}

/**
 * A modpack is an instance. Ask the backend what it needs, create that instance,
 * then hand it to the importer to be filled from the pack's index.
 */
export async function createFromModpack(canonicalId: string, versionId?: string): Promise<boolean> {
  let target;
  try {
    target = await getModpackTarget(canonicalId, versionId);
  } catch (error) {
    toast(errMessage(error), 'error');
    return false;
  }

  const loader = (target.loader || 'vanilla') as LoaderKey;
  const created = await createInstance({
    name: target.name,
    selectionId: target.selection_id,
    icon: defaultIconFor(loader),
    accent: '',
  });
  if (!created.ok || !created.instance) return false;

  const instanceId = created.instance.id;
  toast(`Installing ${target.name}…`, 'info');
  try {
    const report = await installModpack(instanceId, canonicalId, target.version_id);
    const notes = [plural(report.file_count, 'file', 'files')];
    if (report.overrides_applied > 0) notes.push(`${report.overrides_applied} config files`);
    toast(`${report.name} ${report.version} installed · ${notes.join(' · ')}`, 'success');
    if (report.mismatch) toast(report.mismatch, 'info');
  } catch (error) {
    toast(`Created the instance, but ${target.name} failed to install: ${errMessage(error)}`, 'error');
    return false;
  }

  navigate({ name: 'instance', id: instanceId });
  return true;
}
