import { api } from './api';
import { toast } from './toast';
import { errMessage } from './utils';
import { navigate } from './ui-state';
import { addInstance } from './actions';
import { applyInstallQueueResponse } from './machines/downloads';
import { createResultToastMessage, createToastKind, type CreateResultPresentationSource } from './create-presenters';
import type { Instance } from './types-instance';
import type { InstallQueueStateResponse } from './types-install';

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
  instance?: Instance;
  error?: string;
}

interface CreateQueuedInstallSummary {
  state_id?: string;
  kind?: string;
  label?: string;
  queue_id?: string | null;
  install_id?: string | null;
  operation_id?: string | null;
}

interface CreateResponse extends CreateResultPresentationSource {
  id?: string;
  error?: string;
  install_queue?: InstallQueueStateResponse;
  queued_install?: CreateQueuedInstallSummary;
}

function isInstance(value: CreateResponse & Partial<Instance>): value is CreateResponse & Instance {
  return (
    typeof value.id === 'string' &&
    value.id.trim().length > 0 &&
    typeof value.name === 'string' &&
    value.name.trim().length > 0 &&
    typeof value.version_id === 'string' &&
    value.version_id.trim().length > 0 &&
    typeof value.created_at === 'string' &&
    value.created_at.trim().length > 0 &&
    typeof value.view_model?.summary === 'string' &&
    value.view_model.summary.trim().length > 0
  );
}

export async function createInstance(args: CreateInstanceArgs): Promise<CreateInstanceResult> {
  const { selectionId, icon, accent } = args;
  const baseName = args.name.trim();
  if (!baseName) return { ok: false, error: 'Name is required' };
  if (!selectionId) return { ok: false, error: 'Version is required' };

  let res: CreateResponse & Partial<Instance>;
  try {
    const endpoint = args.modpack ? '/instances/modpack' : args.setupPlanId ? '/instances/setup' : '/instances';
    res = (await api('POST', endpoint, {
      ...(args.setupPlanId ? { plan_id: args.setupPlanId } : {}),
      ...(args.modpack ? { canonical_id: args.modpack.canonicalId, version_id: args.modpack.versionId } : {}),
      name: baseName,
      selection_id: selectionId,
      icon,
      accent,
      ...(args.initialSettings ?? {}),
    })) as CreateResponse & Partial<Instance>;
  } catch (err: unknown) {
    const message = errMessage(err);
    toast(`Failed to create instance: ${message}`, 'error');
    return { ok: false, error: message };
  }

  if (res.error || !isInstance(res)) {
    const error = res.error || 'server returned an incomplete instance';
    if (!res.error) console.error('Create instance returned invalid payload');
    toast(`Failed to create instance: ${error}`, 'error');
    return { ok: false, error };
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
