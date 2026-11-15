import { errMessage, toast } from '../app/toast';
import { navigate } from '../app/route';
import { addInstance, instances } from '../instances/state';
import { applyInstallQueueResponse, refreshInstallQueue } from '../install/queue';
import { createResultToastMessage, createToastKind } from './presenters';
import { createFromModpack, createInstance as createInstanceCommand, executeSetup } from './api';
import type { CreateInstanceRequest } from '../generated/CreateInstanceRequest';
import type { CreateInstanceResponse } from '../generated/CreateInstanceResponse';
import type { EnrichedInstance } from '../generated/EnrichedInstance';

export type InitialInstanceSettings = Pick<
  CreateInstanceRequest,
  'max_memory_mb' | 'art_seed' | 'window_width' | 'window_height' | 'jvm_preset_id' | 'auto_optimize'
>;

export interface CreateInstanceArgs {
  name: string;
  selectionId: string;
  icon: string;
  accent: string;
  initialSettings?: InitialInstanceSettings;
  setupPlanId?: string;
  modpack?: { canonicalId: string; versionId: string };
  /** Leave the route where it is: one of several instances being created. */
  stay?: boolean;
}

export interface CreateInstanceResult {
  ok: boolean;
  instance?: EnrichedInstance;
  error?: string;
}

export async function createInstance(args: CreateInstanceArgs): Promise<CreateInstanceResult> {
  const { selectionId, icon, accent } = args;
  const baseName = args.name.trim();
  if (!baseName) return { ok: false, error: 'Name is required' };
  if (!selectionId) return { ok: false, error: 'Version is required' };

  const expectedInstances = instances.value;
  let res: CreateInstanceResponse;
  try {
    const body: CreateInstanceRequest = {
      name: baseName,
      selection_id: selectionId,
      icon,
      accent,
      ...(args.initialSettings ?? {}),
    };
    res = args.modpack
      ? await createFromModpack(args.modpack.canonicalId, args.modpack.versionId, body)
      : args.setupPlanId
        ? await executeSetup(args.setupPlanId, body)
        : await createInstanceCommand(body);
  } catch (err: unknown) {
    const message = errMessage(err);
    toast(`Failed to create instance: ${message}`, 'error');
    return { ok: false, error: message };
  }

  let queueError: string | null = null;
  if (res.install_queue) {
    try {
      await applyInstallQueueResponse(res.install_queue, { connectActive: true });
    } catch (error: unknown) {
      // Creation is already confirmed. A progress connection failure must not
      // leave the create form available to repeat the accepted mutation.
      queueError = errMessage(error);
    }
  }
  let created = instances.value.find((instance) => instance.id === res.id);
  if (!created) {
    if (instances.value === expectedInstances) {
      created = res;
      addInstance(created);
    } else {
      try {
        await refreshInstallQueue({ connectActive: true, requireInstalledState: true });
      } catch (error: unknown) {
        queueError = errMessage(error);
      }
      created = instances.value.find((instance) => instance.id === res.id);
    }
  }
  toast(createResultToastMessage(res.view_model), createToastKind(res.view_model.tone));
  if (queueError) toast(`Instance created, but download status could not be refreshed: ${queueError}`, 'error');
  if (created && !args.stay) navigate({ name: 'instance', id: created.id });

  return { ok: true, instance: created };
}
