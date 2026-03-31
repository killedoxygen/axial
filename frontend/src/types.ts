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
  eventSource?: EventSource;
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
  logExpanded: boolean;
  logHeight: number;
  collapsedGroups: Record<string, boolean>;
  sidebarFilter: string;
  sounds: boolean;
  shortcuts: Record<string, ShortcutBinding>;
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
