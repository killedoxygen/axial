import { api } from './api';
import { toast } from './toast';
import { errMessage } from './utils';
import { navigate } from './ui-state';
import { addInstance } from './actions';
import { applyInstallQueueResponse, refreshInstallQueue } from './machines/downloads';
import type { Instance } from './types-instance';
import type { InstallQueueStateResponse } from './types-install';
import type { ToastKind } from './types-ui';

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
}

export interface CreateInstanceResult {
  ok: boolean;
  instance?: Instance;
  error?: string;
}

interface CreateResultViewModel {
  state_id?: string;
  tone?: string;
  title?: string;
  summary?: string;
  detail?: string | null;
}

interface CreateQueuedInstallSummary {
  state_id?: string;
  kind?: string;
  label?: string;
  queue_id?: string | null;
  install_id?: string | null;
  operation_id?: string | null;
}

interface CreateGuardianNotice {
  state_id?: string;
  tone?: string;
  message?: string;
  detail?: string | null;
}

interface CreateResponse {
  id?: string;
  error?: string;
  view_model?: CreateResultViewModel;
  install_queue?: InstallQueueStateResponse;
  queued_install?: CreateQueuedInstallSummary;
  guardian_notice?: CreateGuardianNotice;
}

function trimmed(value: unknown): string {
  return typeof value === 'string' ? value.trim() : '';
}

function createToastKind(tone: string | undefined): ToastKind {
  if (tone === 'error') return 'error';
  if (tone === 'warn') return 'info';
  return 'success';
}

function appendUnique(parts: string[], value: string): void {
  if (!value || parts.some((part) => part.includes(value))) return;
  parts.push(value);
}

function createResultToastMessage(res: CreateResponse): string {
  const summary = trimmed(res.view_model?.summary);
  const detail = trimmed(res.view_model?.detail);
  const guardianMessage = trimmed(res.guardian_notice?.message);
  const guardianDetail = trimmed(res.guardian_notice?.detail);
  const parts: string[] = [];

  appendUnique(parts, summary);
  appendUnique(parts, guardianMessage);
  appendUnique(parts, detail);
  appendUnique(parts, guardianDetail);
  return parts.join(' ');
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
    res = (await api('POST', '/instances', {
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
    void refreshInstallQueue({ connectActive: true, retryPendingStart: true });
  }
  toast(createResultToastMessage(res), createToastKind(res.view_model?.tone ?? res.guardian_notice?.tone));
  navigate({ name: 'instance', id: created.id });

  return { ok: true, instance: created };
}
