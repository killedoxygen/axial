import type {
  GuardianEvidenceField,
  GuardianFact,
  GuardianTargetDescriptor,
  OperationProofRecord,
} from './types-guardian';

export type PerformanceMode = 'managed' | 'vanilla' | 'custom';

export type InstancePerformanceMode = PerformanceMode | '';

export interface PerformanceProofRecord {
  operation_id?: string | null;
  target: GuardianTargetDescriptor;
  health: string;
  rollback: string;
  fields: GuardianEvidenceField[];
  retention: string;
}

export type ApplicationViewModelTone = 'ok' | 'warn' | 'err' | 'mute';

export interface PerformancePlanSummaryViewModel {
  state_id: string;
  title: string;
  detail: string;
  tone: ApplicationViewModelTone;
  health?: string | null;
  composition_id?: string | null;
  managed_artifact_count: number;
  actions: Array<{
    command: string;
    action?: string | null;
    label: string;
    enabled: boolean;
    disabled_reason?: string | null;
  }>;
}

export interface BenchmarkMatrixModeDescriptor {
  id: string;
  description: string;
  intended_use: string;
}

export interface BenchmarkMatrixRunTypeDescriptor {
  id: string;
  description: string;
}

export interface BenchmarkMatrixProfileDescriptor {
  id: string;
  scenario: string;
  description: string;
  intended_use: string;
}

export interface BenchmarkMatrixTargetDescriptor {
  id: string;
  family: string;
  version: string;
  loader: string;
  profile: string;
  run_type: string;
  description: string;
  intended_use: string;
}

export interface BenchmarkMatrixLimits {
  max_payload_bytes: number;
  custom_post_values_allowed: boolean;
}

export interface BenchmarkMatrixResponse {
  schema: string;
  schema_version: number;
  modes: BenchmarkMatrixModeDescriptor[];
  run_types: BenchmarkMatrixRunTypeDescriptor[];
  profiles: BenchmarkMatrixProfileDescriptor[];
  representative_targets: BenchmarkMatrixTargetDescriptor[];
  limits: BenchmarkMatrixLimits;
}

export interface BenchmarkQualificationSuitePreview {
  present?: boolean;
  suite_id?: string;
  mode?: string;
  run_count?: number;
}

export interface BenchmarkQualificationTargetPreview {
  family: string;
  loader: string;
  version: string;
  mode: string;
}

export interface BenchmarkQualificationRequiredEvidence {
  profile: string;
  run_type: string;
  mode: string;
  performance_mode: PerformanceMode | string;
}

export interface BenchmarkQualificationSuiteRunPreview {
  present: boolean;
  run_index?: number;
  profile?: string;
  run_type?: string;
  target_id?: string;
  benchmark_id?: string;
  session_id?: string;
  state?: string;
}

export interface BenchmarkQualificationProofComparisonPreview {
  present: boolean;
  baseline_session_id?: string;
  metric_name?: string;
  matched_sample_count?: number;
}

export interface BenchmarkQualificationProofPreview {
  present: boolean;
  session_id?: string;
  benchmark_id?: string;
  profile?: string;
  run_type?: string;
  mode?: string;
  performance_mode?: PerformanceMode | string;
  version?: string;
  outcome?: string;
  comparison?: BenchmarkQualificationProofComparisonPreview;
}

export interface BenchmarkQualificationTargetEvidencePreview {
  role: string;
  target_id: string;
  family: string;
  loader: string;
  version: string;
  required: BenchmarkQualificationRequiredEvidence;
  suite_run: BenchmarkQualificationSuiteRunPreview;
  proof: BenchmarkQualificationProofPreview;
  missing: string[];
  view_model: BenchmarkQualificationTargetViewModel;
}

export type BenchmarkViewTone = 'neutral' | 'accent' | 'ok' | 'warn' | 'err' | 'info';

export interface BenchmarkQualificationViewModel {
  status_label: string;
  status_tone: BenchmarkViewTone;
  target_label: string;
  suite_label: string;
  schema_label: string;
  missing_summary: string;
  suite_summary: string;
  evidence_summary: string;
}

export interface BenchmarkQualificationTargetViewModel {
  role_label: string;
  target_label: string;
  required_label: string;
  suite_label: string;
  suite_present: boolean;
  proof_label: string;
  proof_present: boolean;
  missing_label: string;
  missing_tone: BenchmarkViewTone;
}

export interface BenchmarkQualificationPreviewResponse {
  schema: string;
  schema_version: number;
  status: 'ready' | 'incomplete';
  view_model: BenchmarkQualificationViewModel;
  suite: BenchmarkQualificationSuitePreview;
  target: BenchmarkQualificationTargetPreview;
  targets: BenchmarkQualificationTargetEvidencePreview[];
}

export type BenchmarkQualificationResponse = BenchmarkQualificationPreviewResponse;

export interface BenchmarkSuiteDriverStatus {
  id: string;
  state: string;
  suite_id?: string;
  mode?: string;
  interval_ms?: number;
  created_at?: string;
  updated_at?: string;
  active_session_id?: string;
  last_run_index?: number;
  last_session_id?: string;
  error?: string;
}

export interface BenchmarkSuiteDriverSuiteStatus {
  suite_id?: string;
  mode?: string;
  run_count?: number;
  launched_run_count?: number;
  pending_run_index?: number | null;
}

export interface BenchmarkSuiteDriverViewModel {
  state_label: string;
  state_tone: BenchmarkViewTone;
  can_stop: boolean;
  can_resume: boolean;
  can_check_family_c_qualification: boolean;
}

export interface BenchmarkSuiteDriverResponse {
  status: string;
  driver: BenchmarkSuiteDriverStatus;
  suite: BenchmarkSuiteDriverSuiteStatus;
  view_model: BenchmarkSuiteDriverViewModel;
  resumed_from?: string;
}

export interface BenchmarkSuiteDriversResponse {
  status: string;
  drivers: BenchmarkSuiteDriverResponse[];
}

export type CompositionFamily = 'A' | 'B' | 'C' | 'D' | 'E' | 'F';

export type CompositionTier = 'extended' | 'core' | 'vanilla_enhanced';

export type ModCondition = 'always' | 'hardware' | 'version_range' | 'recommend';

export interface PerformanceHardwareRequirement {
  gpu_vendor: string;
  gpu_arch_min: number;
  min_ram_mb: number;
  min_cores: number;
}

export interface ManagedPerformanceMod {
  project_id: string;
  slug: string;
  name: string;
  condition: ModCondition;
  version_range?: string;
  hardware_req?: PerformanceHardwareRequirement | null;
  mutual_exclusions?: string[];
}

export interface PerformancePlanResponse {
  active: boolean;
  effective: EffectivePerformancePlan;
  guardian_facts: GuardianFact[];
  composition_id: string;
  family: CompositionFamily;
  loader: string;
  mode: PerformanceMode;
  tier: CompositionTier;
  mods: ManagedPerformanceMod[];
  jvm_preset?: string;
  fallback_chain?: string[];
  warnings?: string[];
  fallback_reason?: string;
}

export type PerformanceLoaderPosture = 'vanilla' | 'modded_loader';
export type PerformanceContributionSource = 'performance_plan' | 'launcher_policy' | 'user_controlled';
export type PerformanceLaunchSmoothingPolicy = 'managed' | 'launcher_defaults' | 'user_controlled';
export type PerformanceInstrumentationMode = 'not_configured';

export interface EffectivePerformancePlan {
  active: boolean;
  selected_mode: PerformanceMode;
  version_family: CompositionFamily;
  loader: string;
  loader_posture: PerformanceLoaderPosture;
  composition: {
    id: string | null;
    tier: CompositionTier;
    selected: boolean;
    managed_artifact_count: number;
  };
  managed_artifacts: Array<{
    artifact_id: string;
    project_id: string;
    slug: string;
    name: string;
    condition: ModCondition;
  }>;
  jvm_contribution: {
    preset?: string | null;
    source: PerformanceContributionSource;
    explanation: string;
  };
  launch_smoothing: {
    policy: PerformanceLaunchSmoothingPolicy;
    explanation: string;
  };
  instrumentation_policy: {
    policy: PerformanceInstrumentationMode;
    explanation: string;
  };
  fallback: {
    selected: boolean;
    chain: string[];
    reason?: string | null;
    launchable: boolean;
  };
  health_requirements: {
    expected_health: PerformanceHealthStatus;
    expected_tier: CompositionTier;
    requires_composition_lock: boolean;
    expected_managed_artifact_count: number;
    managed_artifact_integrity_required: boolean;
    required_ownership?: 'composition_managed' | 'user_managed' | null;
    rollback_expected: boolean;
  };
  explanation: {
    summary: string;
    details: string[];
  };
}

export type PerformanceHealthStatus = 'healthy' | 'degraded' | 'fallback' | 'disabled' | 'invalid';

export type PerformanceRuleSource = 'built_in' | 'remote';

export type PerformanceRuleChannel = 'bundled' | 'local' | 'remote';

export type PerformanceRulesValidation = 'valid' | 'invalid';

export type PerformanceRulesCacheState = 'recorded' | 'invalid' | 'unavailable';

export interface PerformanceRulesCacheStatus {
  recorded: boolean;
  state: PerformanceRulesCacheState;
  updated_at: string | null;
  loaded_at: string | null;
  warning?: string | null;
}

export type PerformanceOwnershipClass = 'composition_managed' | 'user_managed';

export type PerformanceManagedArtifactProvider = 'modrinth';

export type EmergencyDisableTarget = 'composition' | 'artifact';

export interface PerformanceEmergencyDisable {
  id: string;
  target: EmergencyDisableTarget;
  target_id: string;
  reason: string;
  families: CompositionFamily[];
  loaders: string[];
  tiers: CompositionTier[];
}

export interface PerformanceFamilyCoverage {
  family: CompositionFamily;
  composition_count: number;
  loaders: string[];
  tiers: CompositionTier[];
  managed_mod_count: number;
  warnings: string[];
}

export interface PerformanceRulesStatus {
  rule_source: PerformanceRuleSource;
  rule_channel: PerformanceRuleChannel;
  rules_cache: PerformanceRulesCacheStatus;
  view_model: PerformanceRulesStatusViewModel;
  guardian_facts: GuardianFact[];
  schema_version: number;
  generated_at: string;
  composition_count: number;
  family_coverage: PerformanceFamilyCoverage[];
  remote_refresh: boolean;
  last_refresh_at: string | null;
  validation: PerformanceRulesValidation;
  health_states: PerformanceHealthStatus[];
  ownership_classes: PerformanceOwnershipClass[];
  emergency_disable_count: number;
  emergency_disables: PerformanceEmergencyDisable[];
  warnings: string[];
}

export interface PerformanceRulesStatusViewModel {
  source_label: string;
  channel_label: string;
  validation_label: string;
  validation_tone: 'ok' | 'err';
  validation_icon: string;
  summary: string;
  refresh_label: string;
  generated_label: string;
  cache_label: string;
  emergency_disable_label: string;
  details_label: string;
  health_states_label: string;
  ownership_label: string;
  warnings: string[];
}

export interface PerformanceHealthResponse {
  active: boolean;
  health: PerformanceHealthStatus;
  composition_id: string;
  tier: CompositionTier | '';
  installed_count: number;
  managed_artifacts: PerformanceManagedArtifactSummary[];
  warnings: string[];
  guardian_facts: GuardianFact[];
  proof: PerformanceProofRecord;
  view_model: PerformancePlanSummaryViewModel;
  display: PerformanceInstanceDisplay;
}

export interface PerformanceInstanceDisplay {
  memory: PerformanceMemoryDisplay;
  runtime: PerformanceRuntimeDisplay;
  mode: PerformanceModeDisplay;
}

export interface PerformanceMemoryDisplay {
  min_gb: number;
  max_gb: number;
  label: string;
}

export interface PerformanceRuntimeDisplay {
  detected: boolean;
  label: string;
}

export interface PerformanceModeDisplay {
  mode: PerformanceMode | string;
  label: string;
  source: 'instance' | 'global' | string;
  source_label: string;
}

export interface PerformanceRollbackSnapshotSummary {
  id: string;
  created_at: string;
  composition_id: string;
  tier: CompositionTier;
  installed_count: number;
  artifact_count: number;
  ownership_class: PerformanceOwnershipClass;
  rollback_available: boolean;
  latest: boolean;
}

export interface PerformanceRollbackListResponse {
  snapshots: PerformanceRollbackSnapshotSummary[];
}

export type PerformanceInstallStatus = 'queued' | 'complete' | 'removed' | 'rolled_back';

export interface PerformanceManagedArtifactSummary {
  project_id: string;
  version_id: string;
  filename: string;
  ownership_class: PerformanceOwnershipClass;
  source_provider: PerformanceManagedArtifactProvider;
  sha512_present: boolean;
  sha512_verified: boolean;
}

export interface PerformanceInstallResponse {
  active: boolean;
  status: PerformanceInstallStatus;
  install_id?: string;
  health: PerformanceHealthStatus;
  composition_id: string;
  tier: CompositionTier | '';
  installed_count: number;
  managed_artifacts: PerformanceManagedArtifactSummary[];
  warnings: string[];
}

export interface PerformanceOperationPayload {
  game_version?: string;
  loader?: string;
  mode?: string;
  rollback_id?: string;
}

export type PerformanceOperationAction = 'install' | 'remove' | 'rollback';

export type PerformanceOperationState =
  | 'queued'
  | 'planning'
  | 'applying'
  | 'removing'
  | 'rolling_back'
  | 'complete'
  | 'failed'
  | 'interrupted'
  | string;

export interface PerformanceOperationProgressViewModel {
  phase: string;
  current: number;
  total: number;
  done: boolean;
}

export interface PerformanceOperationViewModel {
  state_label: string;
  tone: ApplicationViewModelTone;
  title: string;
  detail: string;
  progress: PerformanceOperationProgressViewModel;
  is_terminal: boolean;
  is_complete: boolean;
}

export interface PerformanceOperationStatus {
  id: string;
  instance_id: string;
  action: PerformanceOperationAction | string;
  payload: PerformanceOperationPayload;
  state: PerformanceOperationState;
  error?: string;
  created_at: string;
  updated_at: string;
  proof?: OperationProofRecord | null;
  view_model: PerformanceOperationViewModel;
}

export interface PerformanceInstanceOperationResponse {
  operation: PerformanceOperationStatus | null;
}
