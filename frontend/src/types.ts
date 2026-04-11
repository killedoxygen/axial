// ── API response types (mirrors Go structs) ──

export interface Instance {
  id: string;
  name: string;
  version_id: string;
  created_at: string;
  last_played_at?: string;
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
  version_type?: string;
  launchable: boolean;
  status_detail?: string;
  needs_install?: string;
  java_major?: number;
  saves_count: number;
  mods_count: number;
  resource_count: number;
  shader_count: number;
}

export interface Version {
  id: string;
  type: string;
  release_time?: string;
  inherits_from?: string;
  launchable: boolean;
  installed: boolean;
  status: string;
  status_detail?: string;
  needs_install?: string;
  java_component?: string;
  java_major?: number;
  manifest_url?: string;
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
  theme?: string;
  custom_hue?: number;
  custom_vibrancy?: number;
  lightness?: number;
  onboarding_done: boolean;
  mc_dir?: string;
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
  id: string;
  type: string;
  release_time: string;
  url: string;
  installed: boolean;
}

export interface Catalog {
  latest: { release: string; snapshot: string };
  versions: CatalogVersion[];
}

// ── Install types ──

export type LoaderType = 'fabric' | 'quilt' | 'forge' | 'neoforge';

export interface InstallItem {
  versionId: string;
  loader?: {
    type: LoaderType;
    gameVersion: string;
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
  launchedAt: string;
  allocatedMB: number;
  healing?: LaunchHealingSummary;
  eventSource?: EventSource;
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
  advanced_overrides?: boolean;
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

export interface GameVersion {
  version: string;
  stable: boolean;
}

export interface LoaderVersion {
  version: string;
  stable: boolean;
  recommended?: boolean;
}

export interface LoaderInfo {
  type: LoaderType;
  name: string;
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
