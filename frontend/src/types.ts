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
  performance_mode?: string;
  extra_jvm_args?: string;
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
  performance_mode?: string;
  guardian_mode?: string;
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

export interface RunningSession {
  sessionId: string;
  versionId: string;
  pid: number;
  state?: string;
  stopping?: boolean;
  launchedAt: string;
  allocatedMB: number;
  healing?: LaunchHealingSummary;
  guardian?: GuardianSummary;
  eventSource?: EventSource;
}

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
