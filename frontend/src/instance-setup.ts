import { api } from './api';
import { updateInstanceInList } from './actions';
import { createResultToastMessage, createToastKind } from './create-presenters';
import { dtoError, dtoOptionalString, dtoRecord, dtoString } from './dto-contract';
import { enrichedInstanceResponse } from './dto-core';
import { installQueueStateResponse } from './dto-install';
import { refreshInstanceReadiness } from './instance-readiness';
import { applyInstallQueueResponse, refreshInstallQueue } from './machines/downloads';
import { toast } from './toast';
import { errMessage } from './utils';

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
    const payload = await api('POST', `/instances/${encodeURIComponent(instanceId)}/setup/resume`, {});
    const error = dtoError(payload);
    if (error) throw new Error(error);
    const record = dtoRecord(payload, 'Resume instance setup');
    const instance = enrichedInstanceResponse(record);
    if (instance.id !== instanceId) throw new Error('Setup response did not match the requested instance.');
    const view = dtoRecord(record.view_model, 'Resume setup view');
    const viewModel = {
      tone: dtoOptionalString(view.tone, 'Resume setup tone'),
      summary: dtoString(view.summary, 'Resume setup summary'),
      detail: view.detail == null ? null : dtoString(view.detail, 'Resume setup detail'),
    };
    updateInstanceInList(instance);
    toast(createResultToastMessage({ view_model: viewModel }), createToastKind(viewModel.tone));
    // The accepted resume remains valid if queue decoding or connection fails.
    // The queue/readiness owners can recover status without another mutation.
    try {
      if (record.install_queue != null) {
        await applyInstallQueueResponse(installQueueStateResponse(record.install_queue), { connectActive: true });
      }
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
  await Promise.allSettled([
    refreshInstallQueue({ connectActive: true }),
    refreshInstanceReadiness(instanceId),
  ]);
}
