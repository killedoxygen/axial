import type { LaunchActionState } from './types-launch';
import type { InstancePerformanceMode } from './types-performance';
import type { InstallQueueInstallItemViewModel } from './types-install';

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
  install_target?: InstallQueueInstallItemViewModel | null;
  java_major?: number;
  saves_count: number;
  mods_count: number;
  resource_count: number;
  shader_count: number;
  counts_available?: boolean;
}

export type { InstanceWorldInfo as InstanceWorld } from './generated/InstanceWorldInfo';
export type { InstanceModInfo as InstanceMod } from './generated/InstanceModInfo';
export type { InstanceScreenshotInfo as InstanceScreenshot } from './generated/InstanceScreenshotInfo';
export type { InstanceLogInfo as InstanceLogFile } from './generated/InstanceLogInfo';
export type { InstanceResourcesResponse as InstanceResourceSummary } from './generated/InstanceResourcesResponse';
export type { InstanceLogTailResponse as InstanceLogTail } from './generated/InstanceLogTailResponse';
