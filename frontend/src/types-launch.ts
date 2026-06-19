import type { GuardianFact, GuardianMode, GuardianSummary } from './types-guardian';

export type LaunchActionTone = 'ok' | 'warn' | 'err' | 'mute';
export type LaunchPrimaryAction = 'launch' | 'install' | 'blocked';

export interface LaunchActionState {
  state_id: string;
  label: string;
  tone: LaunchActionTone;
  launchable: boolean;
  primary_action: LaunchPrimaryAction;
  disabled_reason?: string;
}

export interface LaunchBenchmarkMetadata {
  id?: string;
  profile?: string;
  run_type?: string;
  mode?: string;
}

export interface RunningSession {
  sessionId: string;
  versionId: string;
  pid: number;
  state?: string;
  stopping?: boolean;
  launchedAt: string;
  allocatedMB: number;
  viewModel?: LaunchStatusViewModel;
  benchmark?: LaunchBenchmarkMetadata;
  healing?: LaunchHealingSummary;
  guardian?: GuardianSummary;
  outcome?: LaunchSessionOutcome;
  eventSource?: EventSource;
}

export type LaunchOverrideOrigin = 'global' | 'instance';

export interface LaunchPreflightOverride {
  present: boolean;
  origin?: LaunchOverrideOrigin;
}

export interface LaunchPreflightMemory {
  max_memory_mb: number;
  min_memory_mb: number;
  min_clamped: boolean;
}

export interface LaunchPreflightOverrides {
  java: LaunchPreflightOverride;
  preset: LaunchPreflightOverride;
  raw_jvm_args: LaunchPreflightOverride;
}

export interface LaunchPreflightResourceBudget {
  active_session_count: number;
  active_install_count: number;
  active_memory_allocation_mb: number;
  requested_memory_mb?: number;
  estimated_remaining_memory_mb?: number;
  memory_pressure: boolean;
  cpu_pressure: boolean;
  install_pressure: boolean;
  disk_pressure: boolean;
}

export type LaunchReadinessReasonId =
  | 'version_json_missing'
  | 'client_jar_missing'
  | 'client_jar_corrupt'
  | 'parent_version_missing'
  | 'incomplete_install'
  | 'libraries_missing'
  | 'libraries_corrupt'
  | 'asset_index_missing'
  | 'asset_index_corrupt'
  | 'managed_runtime_missing'
  | 'java_override_missing';

export type LaunchReadinessSeverity = 'blocking' | 'recoverable';

export interface LaunchReadinessReason {
  id: LaunchReadinessReasonId;
  severity: LaunchReadinessSeverity;
  message: string;
}

export interface LaunchReadiness {
  launchable: boolean;
  reasons: LaunchReadinessReason[];
}

export interface LaunchPreflightResponse {
  status: 'ready';
  guardian: GuardianSummary;
  mode: GuardianMode;
  memory: LaunchPreflightMemory;
  overrides: LaunchPreflightOverrides;
  readiness: LaunchReadiness;
  guardian_facts: GuardianFact[];
  resource_budget: LaunchPreflightResourceBudget;
}

export type HealingEventKind = 'runtime_bypassed' | 'preset_downgraded' | 'fallback_applied';

export interface HealingEvent {
  kind: HealingEventKind;
  detail?: string;
}

export interface LaunchHealingSummary {
  requested_preset?: string;
  effective_preset?: string;
  auth_mode?: string;
  warnings?: string[];
  fallback_applied?: string;
  retry_count?: number;
  failure_class?: string;
  events?: HealingEvent[];
}

export interface InstanceLaunchDraft {
  javaPath: string;
  jvmPreset: string;
  extraJvmArgs: string;
  dirty: boolean;
}

export type LaunchNoticeTone = 'info' | 'success' | 'warned' | 'intervened' | 'error';

export interface LaunchNotice {
  message: string;
  detail?: string;
  details?: string[];
  tone: LaunchNoticeTone;
}

export interface LaunchStatusViewModel {
  state_id: string;
  label: string;
  progress_pct: number;
  terminal: boolean;
}

export type LaunchSessionOutcomeKind = 'clean' | 'stopped' | 'failed' | 'unknown';

export type LaunchSessionExitReason =
  | 'clean_exit'
  | 'external_user_closed'
  | 'launcher_stopped'
  | 'spawn_failed'
  | 'startup_failed'
  | 'startup_stalled'
  | 'watchdog_killed'
  | 'crashed_before_boot'
  | 'crashed_after_boot'
  | 'unknown_exit';

export interface LaunchSessionOutcome {
  reason: LaunchSessionExitReason;
  kind: LaunchSessionOutcomeKind;
  summary: string;
}

export interface LaunchProofScenario {
  scenario_id: string;
  performance_mode: string;
  requested_memory_mb?: number;
  version_id?: string;
  benchmark_profile?: string;
  benchmark_run_type?: string;
  benchmark_mode?: string;
  benchmark_id?: string;
}

export interface LaunchProofDevice {
  tier: string;
  total_memory_mb?: number;
  cpu_threads?: number;
}

export interface LaunchProofComparison {
  baseline_session_id: string;
  baseline_recorded_at: string;
  matched_sample_count: number;
  metric_name: string;
  current_value_ms: number;
  baseline_value_ms: number;
  delta_ms: number;
  delta_percent: number;
}

export type LaunchProofViewTone = 'neutral' | 'accent' | 'ok' | 'warn' | 'err' | 'info';

export interface LaunchProofEvidenceViewModel {
  tone: LaunchProofViewTone;
  label: string;
  detail?: string | null;
}

export interface LaunchProofComparisonViewModel {
  tone: LaunchProofViewTone;
  label: string;
  detail: string;
}

export interface LaunchProofResourceBudgetViewModel {
  pressure_label: string;
  details: string[];
  pressure: boolean;
}

export interface LaunchProofViewModel {
  outcome_label: string;
  outcome_tone: LaunchProofViewTone;
  evidence?: LaunchProofEvidenceViewModel | null;
  comparison: LaunchProofComparisonViewModel;
  resource_budget?: LaunchProofResourceBudgetViewModel | null;
}

export interface LaunchProofResourceBudget {
  host_total_memory_mb?: number;
  host_available_memory_mb?: number;
  host_used_memory_mb?: number;
  host_cpu_threads?: number;
  host_cpu_load_1m_x100?: number;
  host_cpu_load_5m_x100?: number;
  host_cpu_load_15m_x100?: number;
  launcher_process_memory_mb?: number;
  active_session_count: number;
  active_install_count: number;
  active_memory_allocation_mb: number;
  requested_memory_mb?: number;
  estimated_remaining_memory_mb?: number;
  memory_headroom_mb: number;
  memory_pressure: boolean;
  cpu_pressure: boolean;
  install_pressure: boolean;
  launch_disk_available_mb?: number;
  launch_disk_headroom_mb?: number;
  disk_pressure?: boolean;
}

export interface LaunchProofRecord {
  schema: string;
  schema_version: number;
  session_id: string;
  instance_id: string;
  version_id: string;
  launched_at: string;
  recorded_at: string;
  outcome: string;
  session_outcome?: LaunchSessionOutcome | null;
  scenario: LaunchProofScenario;
  device: LaunchProofDevice;
  pid?: number;
  exit_code?: number;
  boot_duration_ms?: number;
  failure_class?: string;
  failure_detail?: string;
  guardian?: GuardianSummary | null;
  healing?: LaunchHealingSummary | null;
  comparison?: LaunchProofComparison | null;
  view_model: LaunchProofViewModel;
  resource_budget?: LaunchProofResourceBudget | null;
}

export interface LaunchReportsResponse {
  reports: LaunchProofRecord[];
}
