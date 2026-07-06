import type { LaunchActionState } from './types-launch';
import type { InstancePerformanceMode } from './types-performance';

export interface Instance {
  id: string;
  name: string;
  version_id: string;
  created_at: string;
  last_played_at?: string;
  art_seed?: number;
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
  launch_action?: LaunchActionState;
}

export interface InstanceVersionDisplay {
  loader_key: string;
  loader_label: string;
  minecraft_label: string;
  loader_version_label: string;
  loader_detail_label: string;
  summary_label: string;
  supports_mods: boolean;
}

export interface EnrichedInstance extends Instance {
  version_display: InstanceVersionDisplay;
  launchable: boolean;
  launch_action: LaunchActionState;
  status_detail?: string;
  needs_install?: string;
  java_major?: number;
  saves_count: number;
  mods_count: number;
  resource_count: number;
  shader_count: number;
}

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
