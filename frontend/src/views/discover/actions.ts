import { getModpackTarget, installContent, planContent } from '../../content';
import { applyInstallQueueResponse } from '../../machines/downloads';
import { plural } from '../../format';
import { toast } from '../../toast';
import { errMessage } from '../../utils';
import { openCreateModpack } from '../../ui-state';
import type { ContentSelection, ResolutionPlan } from '../../types-content';

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
  allowIncompatible = false,
): Promise<AddOutcome> {
  try {
    const queue = await installContent(instanceId, selections, allowIncompatible);
    await applyInstallQueueResponse(queue, { showNotice: true, connectActive: true });
  } catch (error) {
    const message = errMessage(error);
    toast(message, 'error');
    return { status: 'failed', error: message };
  }

  const extra = plan ? plan.items.filter((item) => item.reason === 'dependency' && !item.already_installed).length : 0;
  const suffix = extra > 0 ? ` with ${plural(extra, 'dependency', 'dependencies')}` : '';
  toast(`Queued ${label}${suffix}`, 'success');
  return { status: 'installed', plan };
}

export async function setUpModpack(canonicalId: string, versionId?: string, iconUrl?: string): Promise<boolean> {
  let target;
  try {
    target = await getModpackTarget(canonicalId, versionId);
  } catch (error) {
    toast(errMessage(error), 'error');
    return false;
  }
  openCreateModpack({
    canonical_id: canonicalId,
    version_id: target.version_id,
    name: target.name,
    minecraft: target.minecraft,
    loader: target.loader,
    loader_label: target.loader_label,
    selection_id: target.selection_id,
    icon_url: iconUrl,
  });
  return true;
}
