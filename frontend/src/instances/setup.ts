import { createResultToastMessage, createToastKind } from '../create/presenters';
import { applyInstallQueueResponse, refreshInstallQueue } from '../install/queue';
import { errMessage, toast } from '../app/toast';
import { resumeSetup } from './api';
import { refreshInstanceReadiness } from './readiness';

const resuming = new Map<string, Promise<boolean>>();

/** A repeated click shares the accepted request. An uncertain response is
 * reconciled by reads, never by creating another instance or replaying POST. */
export function resumeInstanceSetup(instanceId: string): Promise<boolean> {
  const pending = resuming.get(instanceId);
  if (pending) return pending;
  const work = resume(instanceId).finally(() => {
    if (resuming.get(instanceId) === work) resuming.delete(instanceId);
  });
  resuming.set(instanceId, work);
  return work;
}

async function resume(instanceId: string): Promise<boolean> {
  try {
    const record = await resumeSetup(instanceId);
    if (record.id !== instanceId) throw new Error('Setup response did not match the requested instance.');
    toast(createResultToastMessage(record.view_model), createToastKind(record.view_model.tone));
    // The accepted resume remains valid if queue decoding or connection fails.
    // The queue/readiness owners can recover status without another mutation.
    try {
      if (record.install_queue) {
        await applyInstallQueueResponse(record.install_queue, { connectActive: true });
      }
      await refreshInstanceReadiness(instanceId);
    } catch (error) {
      toast(`Setup accepted, but download status could not be refreshed: ${errMessage(error)}`, 'error');
      await reconcile(instanceId);
    }
    return true;
  } catch (error) {
    toast(`Could not resume setup: ${errMessage(error)}`, 'error');
    await reconcile(instanceId);
    return false;
  }
}

async function reconcile(instanceId: string): Promise<void> {
  await Promise.allSettled([refreshInstallQueue({ connectActive: true }), refreshInstanceReadiness(instanceId)]);
}
