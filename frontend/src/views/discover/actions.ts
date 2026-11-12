import { installContent, planContent } from '../../content';
import { applyInstallQueueResponse } from '../../machines/downloads';
import { plural } from '../../format';
import { toast } from '../../toast';
import { errMessage } from '../../utils';
import type { ContentSelection, ResolutionPlan } from '../../types-content';

export interface AddOutcome {
  status: 'queued' | 'needs-confirmation' | 'failed' | 'superseded';
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
  // Confirming a reviewed plan replaces exactly the existing files it named.
  const replaceFiles =
    allowIncompatible && plan
      ? plan.conflicts.flatMap((conflict) =>
          conflict.kind === 'existing_file' && conflict.path ? [conflict.path] : [],
        )
      : [];
  try {
    const queue = await installContent(instanceId, selections, allowIncompatible, replaceFiles, label);
    await applyInstallQueueResponse(queue, { showNotice: true, connectActive: true });
  } catch (error) {
    const message = errMessage(error);
    toast(message, 'error');
    return { status: 'failed', error: message };
  }

  const extra = plan ? plan.items.filter((item) => item.reason === 'dependency' && !item.already_installed).length : 0;
  const suffix = extra > 0 ? ` with ${plural(extra, 'dependency', 'dependencies')}` : '';
  toast(`Queued ${label}${suffix}`, 'success');
  return { status: 'queued', plan };
}
