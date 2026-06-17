import type { LaunchAuthMode } from './types-auth';
import type { GuardianMode } from './types-guardian';
import type { PerformanceMode } from './types-performance';

export interface Config {
  username: string;
  launch_auth_mode?: LaunchAuthMode;
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
  telemetry_enabled: boolean;
  discord_rpc_enabled?: boolean;
  discord_rpc_onboarding_seen?: boolean;
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
