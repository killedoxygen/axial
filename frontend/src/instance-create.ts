import { api } from './api';
import { toast } from './toast';
import { errMessage } from './utils';
import { navigate } from './ui-state';
import { addInstance } from './actions';
import { applyInstallQueueResponse } from './machines/downloads';
import { createResultToastMessage, createToastKind, type CreateResultPresentationSource } from './create-presenters';
import type { EnrichedInstance } from './types-instance';
import { dtoError, dtoOptionalString, dtoRecord, dtoString } from './dto-contract';
import { enrichedInstanceResponse } from './dto-core';
import { installQueueStateResponse } from './dto-install';

export interface InitialInstanceSettings {
  max_memory_mb?: number;
  art_seed?: number;
  window_width?: number;
  window_height?: number;
  jvm_preset_id?: string;
  auto_optimize?: boolean;
}

export interface CreateInstanceArgs {
  name: string;
  selectionId: string;
  icon: string;
  accent: string;
  initialSettings?: InitialInstanceSettings;
  setupPlanId?: string;
  modpack?: { canonicalId: string; versionId: string };
}

export interface CreateInstanceResult {
  ok: boolean;
  instance?: EnrichedInstance;
  error?: string;
}

interface CreateResponse extends CreateResultPresentationSource {
  id?: string;
  error?: string;
}

export async function createInstance(args: CreateInstanceArgs): Promise<CreateInstanceResult> {
  const { selectionId, icon, accent } = args;
  const baseName = args.name.trim();
  if (!baseName) return { ok: false, error: 'Name is required' };
  if (!selectionId) return { ok: false, error: 'Version is required' };

  let res: CreateResponse & EnrichedInstance;
  let queueSnapshot: unknown;
  try {
    const endpoint = args.modpack ? '/instances/modpack' : args.setupPlanId ? '/instances/setup' : '/instances';
    const payload = await api('POST', endpoint, {
      ...(args.setupPlanId ? { plan_id: args.setupPlanId } : {}),
      ...(args.modpack ? { canonical_id: args.modpack.canonicalId, version_id: args.modpack.versionId } : {}),
      name: baseName,
      selection_id: selectionId,
      icon,
      accent,
      ...(args.initialSettings ?? {}),
    });
    const responseError = dtoError(payload);
    if (responseError) throw new Error(responseError);
    const record = dtoRecord(payload, 'Create instance');
    const view = dtoRecord(record.view_model, 'Create instance view');
    res = {
      ...enrichedInstanceResponse(record),
      view_model: {
        state_id: dtoOptionalString(view.state_id, 'Create result state'),
        tone: dtoOptionalString(view.tone, 'Create result tone'),
        title: dtoOptionalString(view.title, 'Create result title'),
        summary: dtoString(view.summary, 'Create result summary'),
        detail: view.detail == null ? null : dtoString(view.detail, 'Create result detail'),
      },
    };
    queueSnapshot = record.install_queue;
  } catch (err: unknown) {
    const message = errMessage(err);
    toast(`Failed to create instance: ${message}`, 'error');
    return { ok: false, error: message };
  }

  const created = res;
  addInstance(created);
  let queueError: string | null = null;
  if (queueSnapshot != null) {
    try {
      await applyInstallQueueResponse(installQueueStateResponse(queueSnapshot), { connectActive: true });
    } catch (error: unknown) {
      // Creation is already confirmed. A progress connection failure must not
      // leave the create form available to repeat the accepted mutation.
      queueError = errMessage(error);
    }
  }
  toast(createResultToastMessage(res), createToastKind(res.view_model?.tone));
  if (queueError) toast(`Instance created, but download status could not be refreshed: ${queueError}`, 'error');
  navigate({ name: 'instance', id: created.id });

  return { ok: true, instance: created };
}
