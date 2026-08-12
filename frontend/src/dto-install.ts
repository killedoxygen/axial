import { dtoArray, dtoBoolean, dtoEnum, dtoNumber, dtoOptionalNumber, dtoRecord, dtoString } from './dto-contract';
import type {
  InstallActionViewModel,
  InstallFailureViewModel,
  InstallProgressViewModel,
  InstallQueueActiveViewModel,
  InstallQueueContentAction,
  InstallQueueContentItemViewModel,
  InstallQueueInstallItemViewModel,
  InstallQueueNoticeViewModel,
  InstallQueueStateResponse,
  InstallQueueViewModel,
  InstallQueuedItemViewModel,
  InstallStartResponse,
  InstallStatusResponse,
} from './types-install';

const CONTENT_KINDS = ['mod', 'modpack', 'resource_pack', 'shader_pack'] as const;
const LOADER_IDS = [
  'net.fabricmc.fabric-loader',
  'org.quiltmc.quilt-loader',
  'net.minecraftforge',
  'net.neoforged',
] as const;

function nullableString(value: unknown, label: string): string | null | undefined {
  if (value === undefined) return undefined;
  return value === null ? null : dtoString(value, label);
}

function actionResponse(value: unknown): InstallActionViewModel {
  const record = dtoRecord(value, 'Install action');
  return {
    action: dtoString(record.action, 'Install action id'),
    label: dtoString(record.label, 'Install action label'),
    enabled: dtoBoolean(record.enabled, 'Install action enabled'),
    disabled_reason: nullableString(record.disabled_reason, 'Install action disabled reason'),
  };
}

export function installProgressViewModelResponse(value: unknown): InstallProgressViewModel {
  const record = dtoRecord(value, 'Install progress');
  const activeStep = record.active_step == null ? null : dtoRecord(record.active_step, 'Install progress step');
  return {
    phase_id: dtoString(record.phase_id, 'Install progress phase'),
    label: dtoString(record.label, 'Install progress label'),
    progress_pct: dtoNumber(record.progress_pct, 'Install progress percent'),
    terminal: dtoBoolean(record.terminal, 'Install progress terminal'),
    failed: dtoBoolean(record.failed, 'Install progress failed'),
    active_step: activeStep
      ? {
          phase_id: dtoString(activeStep.phase_id, 'Install step phase'),
          label: dtoString(activeStep.label, 'Install step label'),
          progress_pct: dtoNumber(activeStep.progress_pct, 'Install step percent'),
          current: dtoOptionalNumber(activeStep.current, 'Install step current'),
          total: dtoOptionalNumber(activeStep.total, 'Install step total'),
        }
      : null,
  };
}

function failureResponse(value: unknown): InstallFailureViewModel {
  const record = dtoRecord(value, 'Install failure');
  return {
    state_id: dtoString(record.state_id, 'Install failure state'),
    title: dtoString(record.title, 'Install failure title'),
    tone: dtoString(record.tone, 'Install failure tone'),
    summary: dtoString(record.summary, 'Install failure summary'),
    detail: nullableString(record.detail, 'Install failure detail'),
    details:
      record.details == null
        ? []
        : dtoArray(record.details, 'Install failure details').map((detail) =>
            dtoString(detail, 'Install failure detail'),
          ),
    retry_action: actionResponse(record.retry_action),
    dismiss_action: actionResponse(record.dismiss_action),
  };
}

function contentActionResponse(value: unknown): InstallQueueContentAction {
  const record = dtoRecord(value, 'Install content action');
  const kind = dtoEnum(record.kind, 'Install content action kind', ['install', 'uninstall', 'modpack'] as const);
  if (kind === 'install') {
    return {
      kind,
      selections: dtoArray(record.selections, 'Install content selections').map((selection) => {
        const item = dtoRecord(selection, 'Install content selection');
        return {
          canonical_id: dtoString(item.canonical_id, 'Content selection id'),
          kind: dtoEnum(item.kind, 'Content selection kind', CONTENT_KINDS),
          version_id: nullableString(item.version_id, 'Content selection version'),
        };
      }),
      allow_incompatible: dtoBoolean(record.allow_incompatible, 'Install incompatible allowance'),
    };
  }
  if (kind === 'uninstall') {
    return {
      kind,
      canonical_ids: dtoArray(record.canonical_ids, 'Uninstall content ids').map((id) =>
        dtoString(id, 'Uninstall content id'),
      ),
    };
  }
  return {
    kind,
    canonical_id: dtoString(record.canonical_id, 'Modpack id'),
    version_id: dtoString(record.version_id, 'Modpack version'),
    selected_file_ids: dtoArray(record.selected_file_ids, 'Modpack selected files').map((id) =>
      dtoString(id, 'Modpack selected file'),
    ),
    include_overrides: dtoBoolean(record.include_overrides, 'Modpack overrides'),
  };
}

function contentItemResponse(value: unknown): InstallQueueContentItemViewModel {
  const record = dtoRecord(value, 'Install content item');
  return {
    instance_id: dtoString(record.instance_id, 'Install content instance'),
    label: dtoString(record.label, 'Install content label'),
    action: contentActionResponse(record.action),
  };
}

function installItemResponse(value: unknown): InstallQueueInstallItemViewModel {
  const record = dtoRecord(value, 'Install queue item');
  const loader = record.loader == null ? null : dtoRecord(record.loader, 'Install loader item');
  return {
    version_id: dtoString(record.version_id, 'Install version'),
    loader: loader
      ? {
          component_id: dtoEnum(loader.component_id, 'Loader component', LOADER_IDS),
          build_id: dtoString(loader.build_id, 'Loader build'),
          minecraft_version: dtoString(loader.minecraft_version, 'Loader Minecraft version'),
          loader_version: dtoString(loader.loader_version, 'Loader version'),
        }
      : null,
    content: record.content == null ? null : contentItemResponse(record.content),
  };
}

function queuedItemResponse(value: unknown): InstallQueuedItemViewModel {
  const record = dtoRecord(value, 'Queued install');
  return {
    queue_id: dtoString(record.queue_id, 'Queued install id'),
    state_id: dtoString(record.state_id, 'Queued install state'),
    kind: dtoEnum(record.kind, 'Queued install kind', ['vanilla', 'loader', 'content'] as const),
    title: dtoString(record.title, 'Queued install title'),
    label: dtoString(record.label, 'Queued install label'),
    summary: dtoString(record.summary, 'Queued install summary'),
    detail: dtoString(record.detail, 'Queued install detail'),
    position: dtoNumber(record.position, 'Queued install position'),
    total: dtoNumber(record.total, 'Queued install total'),
    install_item: installItemResponse(record.install_item),
    remove_action: actionResponse(record.remove_action),
  };
}

function queueViewResponse(value: unknown): InstallQueueViewModel {
  const record = dtoRecord(value, 'Install queue view');
  return {
    state_id: dtoString(record.state_id, 'Install queue state'),
    status_label: dtoString(record.status_label, 'Install queue status'),
    title: dtoString(record.title, 'Install queue title'),
    summary: dtoString(record.summary, 'Install queue summary'),
    queued_count: dtoNumber(record.queued_count, 'Install queue count'),
    queued_count_label: dtoString(record.queued_count_label, 'Install queue count label'),
    queued_item_label: dtoString(record.queued_item_label, 'Install queue item label'),
    next_label: nullableString(record.next_label, 'Install queue next label'),
    active_queued_count_label: nullableString(record.active_queued_count_label, 'Install active count label'),
    section_title: dtoString(record.section_title, 'Install queue section title'),
    empty_title: dtoString(record.empty_title, 'Install queue empty title'),
    empty_summary: dtoString(record.empty_summary, 'Install queue empty summary'),
  };
}

function startResponse(value: unknown): InstallStartResponse {
  const record = dtoRecord(value, 'Install start');
  return {
    install_id: dtoString(record.install_id, 'Install id'),
    operation_id: dtoString(record.operation_id, 'Install operation'),
    view_model: installProgressViewModelResponse(record.view_model),
  };
}

function activeResponse(value: unknown): InstallQueueActiveViewModel {
  const record = dtoRecord(value, 'Active install');
  return {
    queue_id: dtoString(record.queue_id, 'Active queue id'),
    install_id: nullableString(record.install_id, 'Active install id'),
    operation_id: nullableString(record.operation_id, 'Active operation id'),
    install_started_at_ms:
      record.install_started_at_ms == null ? null : dtoNumber(record.install_started_at_ms, 'Install start time'),
    kind: dtoEnum(record.kind, 'Active install kind', ['vanilla', 'loader', 'content'] as const),
    title: dtoString(record.title, 'Active install title'),
    label: dtoString(record.label, 'Active install label'),
    summary: dtoString(record.summary, 'Active install summary'),
    install_item: installItemResponse(record.install_item),
    progress: installProgressViewModelResponse(record.progress),
  };
}

function noticeResponse(value: unknown): InstallQueueNoticeViewModel {
  const record = dtoRecord(value, 'Install queue notice');
  return {
    state_id: dtoString(record.state_id, 'Install notice state'),
    tone: dtoString(record.tone, 'Install notice tone'),
    message: dtoString(record.message, 'Install notice message'),
    detail: nullableString(record.detail, 'Install notice detail'),
  };
}

export function installQueueStateResponse(value: unknown): InstallQueueStateResponse {
  const record = dtoRecord(value, 'Install queue');
  return {
    active: record.active == null ? null : activeResponse(record.active),
    items: dtoArray(record.items, 'Install queue items').map(queuedItemResponse),
    view_model: queueViewResponse(record.view_model),
    notice: record.notice == null ? null : noticeResponse(record.notice),
    started_install: record.started_install == null ? null : startResponse(record.started_install),
    removed_instance_id: nullableString(record.removed_instance_id, 'Removed instance id'),
  };
}

export function installStatusResponse(value: unknown): InstallStatusResponse {
  const record = dtoRecord(value, 'Install status');
  return {
    install_id: dtoString(record.install_id, 'Install status id'),
    operation_id: dtoString(record.operation_id, 'Install status operation'),
    done: dtoBoolean(record.done, 'Install status done'),
    view_model: installProgressViewModelResponse(record.view_model),
    failure_view_model: record.failure_view_model == null ? null : failureResponse(record.failure_view_model),
  };
}
