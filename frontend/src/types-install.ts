import type { LoaderComponentId } from './types-loader';

export interface InstallItem {
  versionId: string;
  loader?: {
    componentId: LoaderComponentId;
    buildId: string;
    minecraftVersion: string;
    loaderVersion: string;
  };
  content?: InstallQueueContentItemViewModel;
}

export interface InstallProgressStepViewModel {
  phase_id: string;
  label: string;
  progress_pct: number;
  current?: number;
  total?: number;
}

export interface InstallProgressViewModel {
  phase_id: string;
  label: string;
  progress_pct: number;
  terminal: boolean;
  failed: boolean;
  active_step?: InstallProgressStepViewModel | null;
}

export interface InstallGuardianOutcome {
  diagnosis_id: string;
  decision: string;
  label: string;
  detail?: string;
  guidance?: string[];
}

export interface InstallActionViewModel {
  action: string;
  label: string;
  enabled: boolean;
  disabled_reason?: string | null;
}

export interface InstallFailureViewModel {
  state_id: string;
  title: string;
  tone: string;
  summary: string;
  detail?: string | null;
  details: string[];
  retry_action: InstallActionViewModel;
  dismiss_action: InstallActionViewModel;
  repair_action: InstallActionViewModel;
}

export interface InstallGuardianRepairSummary {
  repair_operation_id: string;
  diagnosis_id: string;
  status: string;
  label: string;
  detail?: string | null;
}

export interface InstallStatusResponse {
  install_id: string;
  operation_id: string;
  done: boolean;
  progress: unknown[];
  view_model: InstallProgressViewModel;
  failure_view_model?: InstallFailureViewModel | null;
  failure_point?: string | null;
  guardian?: InstallGuardianOutcome | null;
  guardian_repair?: InstallGuardianRepairSummary | null;
  proof?: unknown;
}

export interface InstallStartResponse {
  install_id: string;
  operation_id: string;
  view_model: InstallProgressViewModel;
}

export interface InstallQueueRequest {
  kind: 'vanilla' | 'loader' | 'content';
  version_id?: string;
  manifest_url?: string;
  component_id?: LoaderComponentId;
  build_id?: string;
  instance_id?: string;
  label?: string;
  content_action?: InstallQueueContentAction;
}

export interface InstallQueueLoaderItemViewModel {
  component_id: LoaderComponentId;
  build_id: string;
  minecraft_version: string;
  loader_version: string;
}

export interface InstallQueueInstallItemViewModel {
  version_id: string;
  loader?: InstallQueueLoaderItemViewModel | null;
  content?: InstallQueueContentItemViewModel | null;
}

export interface InstallQueueContentSelection {
  canonical_id: string;
  kind: 'mod' | 'modpack' | 'resource_pack' | 'shader_pack';
  version_id?: string | null;
}

export type InstallQueueContentAction =
  | {
      kind: 'install';
      selections: InstallQueueContentSelection[];
      allow_incompatible: boolean;
    }
  | { kind: 'uninstall'; canonical_ids: string[] }
  | {
      kind: 'modpack';
      canonical_id: string;
      version_id: string;
      selected_paths: string[];
      include_overrides: boolean;
    };

export interface InstallQueueContentItemViewModel {
  instance_id: string;
  action: InstallQueueContentAction;
}

export interface InstallQueuedItemViewModel {
  queue_id: string;
  state_id: string;
  kind: 'vanilla' | 'loader' | 'content';
  title: string;
  label: string;
  summary: string;
  detail: string;
  position: number;
  total: number;
  install_item: InstallQueueInstallItemViewModel;
  remove_action: InstallActionViewModel;
}

export interface InstallQueueActiveViewModel {
  queue_id: string;
  install_id?: string | null;
  operation_id?: string | null;
  install_started_at_ms?: number | null;
  kind: 'vanilla' | 'loader' | 'content';
  title: string;
  label: string;
  summary: string;
  install_item: InstallQueueInstallItemViewModel;
  progress: InstallProgressViewModel;
}

export interface InstallQueueViewModel {
  state_id: string;
  status_label: string;
  title: string;
  summary: string;
  queued_count: number;
  queued_count_label: string;
  queued_item_label: string;
  next_label?: string | null;
  active_queued_count_label?: string | null;
  section_title: string;
  empty_title: string;
  empty_summary: string;
}

export interface InstallQueueNoticeViewModel {
  state_id: string;
  tone: string;
  message: string;
  detail?: string | null;
}

export interface InstallQueueStateResponse {
  active?: InstallQueueActiveViewModel | null;
  items: InstallQueuedItemViewModel[];
  view_model: InstallQueueViewModel;
  notice?: InstallQueueNoticeViewModel | null;
  started_install?: InstallStartResponse | null;
  removed_instance_id?: string | null;
}
