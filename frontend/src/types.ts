// ── API response types (mirrors Rust structs) ──

export interface Instance {
  id: string;
  name: string;
  version_id: string;
  created_at: string;
  last_played_at?: string;
  art_seed?: number;
  art_preset?: string;
  max_memory_mb?: number;
  min_memory_mb?: number;
  java_path?: string;
  window_width?: number;
  window_height?: number;
  jvm_preset?: string;
  performance_mode?: InstancePerformanceMode;
  extra_jvm_args?: string;
  icon?: string;
  accent?: string;
}

export interface EnrichedInstance extends Instance {
  launchable: boolean;
  status_detail?: string;
  needs_install?: string;
  java_major?: number;
  saves_count: number;
  mods_count: number;
  resource_count: number;
  shader_count: number;
}

export type LifecycleChannel =
  | 'stable'
  | 'preview'
  | 'experimental'
  | 'legacy'
  | 'unknown';

export type LifecycleLabel =
  | 'release'
  | 'recommended'
  | 'latest'
  | 'snapshot'
  | 'pre_release'
  | 'release_candidate'
  | 'beta'
  | 'alpha'
  | 'old_beta'
  | 'old_alpha'
  | 'nightly'
  | 'dev'
  | 'unknown';

export interface LifecycleMeta {
  channel: LifecycleChannel;
  labels: LifecycleLabel[];
  default_rank: number;
  badge_text: string;
  provider_terms: string[];
}

export interface MinecraftVersionMeta {
  family: string;
  base_id: string;
  effective_version: string;
  variant_of: string;
  variant_kind: string;
  display_name: string;
  display_hint: string;
}

export type VersionSubjectKind = 'installed_version' | 'minecraft_version';
export type LoaderBuildSubjectKind = 'loader_build';

export type LoaderComponentId =
  | 'net.fabricmc.fabric-loader'
  | 'org.quiltmc.quilt-loader'
  | 'net.minecraftforge'
  | 'net.neoforged';

export type LoaderType = 'fabric' | 'quilt' | 'forge' | 'neoforge';

export type LoaderTerm =
  | 'recommended'
  | 'latest'
  | 'snapshot'
  | 'pre_release'
  | 'release_candidate'
  | 'beta'
  | 'alpha'
  | 'nightly'
  | 'dev';

export type LoaderTermSource =
  | 'explicit_version_label'
  | 'explicit_api_flag'
  | 'promotion_marker'
  | 'none';

export interface LoaderTermEvidence {
  term: LoaderTerm;
  source: LoaderTermSource;
}

export type LoaderSelectionReason =
  | 'recommended'
  | 'latest_stable'
  | 'latest'
  | 'stable'
  | 'unlabeled'
  | 'latest_unstable'
  | 'unstable'
  | 'unknown';

export type LoaderSelectionSource =
  | 'explicit_version_label'
  | 'explicit_api_flag'
  | 'promotion_marker'
  | 'absence_of_recommended'
  | 'none';

export interface LoaderSelectionMeta {
  default_rank: number;
  reason: LoaderSelectionReason;
  source: LoaderSelectionSource;
}

export interface LoaderBuildMetadata {
  terms: LoaderTerm[];
  evidence: LoaderTermEvidence[];
  selection: LoaderSelectionMeta;
  display_tags: string[];
}

export interface VersionLoaderAttachment {
  component_id: LoaderComponentId;
  component_name: string;
  build_id: string;
  loader_version: string;
  build_meta: LoaderBuildMetadata;
}

export interface Version {
  subject_kind: VersionSubjectKind;
  id: string;
  raw_kind: string;
  release_time?: string;
  minecraft_meta: MinecraftVersionMeta;
  lifecycle: LifecycleMeta;
  inherits_from?: string;
  launchable: boolean;
  installed: boolean;
  status: string;
  status_detail?: string;
  needs_install?: string;
  java_component?: string;
  java_major?: number;
  manifest_url?: string;
  loader?: VersionLoaderAttachment | null;
}

export interface Config {
  username: string;
  max_memory_mb: number;
  min_memory_mb: number;
  java_path_override?: string;
  window_width?: number;
  window_height?: number;
  jvm_preset?: string;
  performance_mode?: PerformanceMode;
  guardian_mode?: GuardianMode;
  theme?: string;
  custom_hue?: number;
  custom_vibrancy?: number;
  lightness?: number;
  onboarding_done: boolean;
  library_dir?: string;
  library_mode?: string;
  music_enabled?: boolean;
  music_volume?: number;
  music_track?: number;
}

export interface SystemInfo {
  total_memory_mb: number;
  recommended_min_mb: number;
  recommended_max_mb: number;
  max_allocatable_gb: number;
}

export interface CatalogVersion {
  subject_kind: VersionSubjectKind;
  id: string;
  raw_kind: string;
  release_time: string;
  minecraft_meta: MinecraftVersionMeta;
  lifecycle: LifecycleMeta;
  url: string;
  installed: boolean;
}

export interface Catalog {
  latest: { release: string; snapshot: string };
  versions: CatalogVersion[];
}

// ── Install types ──

export interface InstallItem {
  versionId: string;
  loader?: {
    componentId: LoaderComponentId;
    buildId: string;
    minecraftVersion: string;
    loaderVersion: string;
  };
}

export interface ActiveInstall {
  versionId: string;
  pct: number;
  label: string;
}

// ── Launch / session types ──

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
  benchmark?: LaunchBenchmarkMetadata;
  healing?: LaunchHealingSummary;
  guardian?: GuardianSummary;
  eventSource?: EventSource;
}

export type PerformanceMode = 'managed' | 'vanilla' | 'custom';

export type InstancePerformanceMode = PerformanceMode | '';

export type GuardianMode = 'managed' | 'custom';

export type GuardianDecision = 'allowed' | 'warned' | 'blocked' | 'intervened';

export type GuardianInterventionKind =
  | 'switch_managed_runtime'
  | 'strip_jvm_args'
  | 'downgrade_preset'
  | 'disable_custom_gc';

export interface GuardianIntervention {
  kind: GuardianInterventionKind;
  detail?: string;
  silent?: boolean;
}

export interface GuardianSummary {
  mode: GuardianMode;
  decision: GuardianDecision;
  message?: string;
  details?: string[];
  guidance?: string[];
  interventions?: GuardianIntervention[];
}

export type HealingEventKind =
  | 'runtime_bypassed'
  | 'preset_downgraded'
  | 'startup_stalled'
  | 'fallback_applied';

export interface HealingEvent {
  kind: HealingEventKind;
  detail?: string;
}

export interface LaunchHealingSummary {
  requested_preset?: string;
  effective_preset?: string;
  requested_java_path?: string;
  effective_java_path?: string;
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

export type LaunchNoticeTone = 'info' | 'success' | 'error';

export interface LaunchNotice {
  message: string;
  detail?: string;
  details?: string[];
  tone: LaunchNoticeTone;
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
  scenario: LaunchProofScenario;
  device: LaunchProofDevice;
  pid?: number;
  exit_code?: number;
  boot_duration_ms?: number;
  failure_class?: string;
  failure_detail?: string;
  comparison?: LaunchProofComparison | null;
  resource_budget?: LaunchProofResourceBudget | null;
}

export interface LaunchReportsResponse {
  reports: LaunchProofRecord[];
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
  limits: BenchmarkMatrixLimits;
}

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

export interface BenchmarkSuiteDriverResponse {
  status: string;
  driver: BenchmarkSuiteDriverStatus;
  suite: BenchmarkSuiteDriverSuiteStatus;
  resumed_from?: string;
}

export interface BenchmarkSuiteDriversResponse {
  status: string;
  drivers: BenchmarkSuiteDriverResponse[];
}

// ── Performance program ──

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

export type PerformanceHealthStatus =
  | 'healthy'
  | 'degraded'
  | 'fallback'
  | 'disabled'
  | 'invalid';

export type PerformanceRuleSource = 'built_in' | 'remote';

export type PerformanceRuleChannel = 'bundled' | 'local' | 'remote';

export type PerformanceRulesValidation = 'valid' | 'invalid';

export type PerformanceRulesCacheState = 'recorded' | 'recovered' | 'unavailable';

export interface PerformanceRulesCacheStatus {
  recorded: boolean;
  state: PerformanceRulesCacheState;
  updated_at: string | null;
  loaded_at: string | null;
  warning?: string | null;
}

export type PerformanceOwnershipClass = 'composition_managed' | 'user_managed';

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

export interface PerformanceHealthResponse {
  active: boolean;
  health: PerformanceHealthStatus;
  composition_id: string;
  tier: CompositionTier | '';
  installed_count: number;
  warnings: string[];
}

export type PerformanceInstallStatus = 'queued' | 'complete' | 'removed' | 'rolled_back';

export interface PerformanceInstallResponse {
  active: boolean;
  status: PerformanceInstallStatus;
  install_id?: string;
  health: PerformanceHealthStatus;
  composition_id: string;
  tier: CompositionTier | '';
  installed_count: number;
  warnings: string[];
}

// ── Version info (detail panel) ──

export interface WorldInfo {
  name: string;
  size: number;
  last_played?: string;
}

export interface SharedDataInfo {
  name: string;
  count: number;
  size: number;
}

export interface VersionInfo {
  id: string;
  folder_size: number;
  dependents: string[];
  worlds: WorldInfo[];
  shared_data: SharedDataInfo[];
}

// ── Instance resource detail ──

export interface InstanceWorld {
  name: string;
  size: number;
  modified_at: string;
}

export interface InstanceMod {
  name: string;
  size: number;
  modified_at: string;
  enabled: boolean;
}

export interface InstanceScreenshot {
  name: string;
  size: number;
  modified_at: string;
}

export interface InstanceLogFile {
  name: string;
  size: number;
  modified_at: string;
}

export interface InstanceResourceSummary {
  worlds: InstanceWorld[];
  mods: InstanceMod[];
  screenshots: InstanceScreenshot[];
  logs: InstanceLogFile[];
  worlds_count: number;
  mods_count: number;
  screenshots_count: number;
  logs_count: number;
}

export interface InstanceLogTail {
  name: string;
  size: number;
  truncated: boolean;
  text: string;
}

// ── UI types ──

export type Page = 'launcher' | 'settings';

export type SidebarFilter = 'all' | 'release' | 'snapshot' | 'modded';

export interface ShortcutBinding {
  key: string;
  ctrl?: boolean;
  shift?: boolean;
  alt?: boolean;
  meta?: boolean;
  desc: string;
}

export interface LocalPrefs {
  theme: string;
  customHue: number;
  customVibrancy: number;
  lightness: number;
  sidebarCompact: boolean;
  logHeight: number;
  collapsedGroups: Record<string, boolean>;
  sidebarFilter: string;
  sounds: boolean;
  shortcuts: Record<string, ShortcutBinding>;
  lastUpdateCheckAt: string;
  dismissedUpdateVersion: string;
}

export type ToastKind = 'success' | 'error';

export interface ToastItem {
  id: number;
  message: string;
  type: ToastKind;
}

// ── Loader metadata ──

export interface LoaderAvailability {
  fresh: boolean;
  stale: boolean;
  cache_hit: boolean;
  checked_at_ms: number;
  last_success_at_ms?: number;
  last_error?: string;
}

export interface LoaderCatalogState {
  availability: LoaderAvailability;
}

export interface LoaderComponentRecord {
  id: LoaderComponentId;
  name: string;
}

export interface LoaderBuildRecord {
  subject_kind: LoaderBuildSubjectKind;
  component_id: LoaderComponentId;
  component_name: string;
  build_id: string;
  minecraft_version: string;
  loader_version: string;
  version_id: string;
  build_meta: LoaderBuildMetadata;
  strategy: string;
  artifact_kind: string;
  installability: string;
}

export interface LoaderBuildsResponse {
  builds: LoaderBuildRecord[];
  catalog: LoaderCatalogState;
}

export interface LoaderGameVersion {
  subject_kind: VersionSubjectKind;
  id: string;
  release_time?: string;
  minecraft_meta: MinecraftVersionMeta;
  lifecycle: LifecycleMeta;
}

export interface LoaderGameVersionsResponse {
  versions: LoaderGameVersion[];
  catalog: LoaderCatalogState;
}

export interface LoaderComponentsResponse {
  components: LoaderComponentRecord[];
}

export type UpdateKind = 'none' | 'release-page' | 'appimage';

export interface UpdateInfo {
  current_version: string;
  latest_version: string;
  available: boolean;
  platform: string;
  arch: string;
  kind: UpdateKind;
  notes_url: string;
  action_url: string;
  action_label: string;
  checked_at: string;
}
