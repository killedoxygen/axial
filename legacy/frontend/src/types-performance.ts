export type PerformanceMode = 'managed' | 'vanilla' | 'custom';

export type InstancePerformanceMode = PerformanceMode | '';

export type ApplicationViewModelTone = 'ok' | 'warn' | 'err' | 'mute';

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

export type PerformanceHealthStatus = 'healthy' | 'disabled' | 'invalid';

export interface PerformanceHealthResponse {
  health: PerformanceHealthStatus;
  view_model: {
    tone: ApplicationViewModelTone;
    title: string;
    detail: string;
  };
}
