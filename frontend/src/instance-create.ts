import { api } from './api';
import { toast } from './toast';
import { errMessage } from './utils';
import { navigate } from './ui-state';
import { addInstance } from './actions';
import { applyInstallQueueResponse } from './machines/downloads';
import { createResultToastMessage, createToastKind, type CreateResultPresentationSource } from './create-presenters';
import type { EnrichedInstance } from './types-instance';
import type { InstallQueueStateResponse } from './types-install';
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
  install_queue?: InstallQueueStateResponse;
}

export async function createInstance(args: CreateInstanceArgs): Promise<CreateInstanceResult> {
  const { selectionId, icon, accent } = args;
  const baseName = args.name.trim();
  if (!baseName) return { ok: false, error: 'Name is required' };
  if (!selectionId) return { ok: false, error: 'Version is required' };

  let res: CreateResponse & EnrichedInstance;
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
    const guardian =
      record.guardian_notice == null ? null : dtoRecord(record.guardian_notice, 'Create Guardian notice');
    res = {
      ...enrichedInstanceResponse(record),
      view_model: {
        state_id: dtoOptionalString(view.state_id, 'Create result state'),
        tone: dtoOptionalString(view.tone, 'Create result tone'),
        title: dtoOptionalString(view.title, 'Create result title'),
        summary: dtoString(view.summary, 'Create result summary'),
        detail: view.detail == null ? null : dtoString(view.detail, 'Create result detail'),
      },
      guardian_notice: guardian
        ? {
            state_id: dtoOptionalString(guardian.state_id, 'Create Guardian state'),
            tone: dtoOptionalString(guardian.tone, 'Create Guardian tone'),
            message: dtoOptionalString(guardian.message, 'Create Guardian message'),
            detail: guardian.detail == null ? null : dtoString(guardian.detail, 'Create Guardian detail'),
          }
        : undefined,
      install_queue: record.install_queue == null ? undefined : installQueueStateResponse(record.install_queue),
    };
  } catch (err: unknown) {
    const message = errMessage(err);
    toast(`Failed to create instance: ${message}`, 'error');
    return { ok: false, error: message };
  }

  const created = res;
  addInstance(created);
  if (res.install_queue) {
    await applyInstallQueueResponse(res.install_queue, { connectActive: true });
  }
  toast(createResultToastMessage(res), createToastKind(res.view_model?.tone ?? res.guardian_notice?.tone));
  navigate({ name: 'instance', id: created.id });

  return { ok: true, instance: created };
}
