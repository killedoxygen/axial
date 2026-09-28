import type { LaunchAuthMode } from './types-auth';
import type { GuardianMode } from './types-guardian';
import type { PerformanceMode } from './types-performance';

export interface Config {
  revision: number;
  username: string;
  launch_auth_mode: LaunchAuthMode;
  max_memory_mb: number;
  min_memory_mb: number;
  java_path_override: string;
  window_width: number;
  window_height: number;
  jvm_preset:
    | ''
    | 'smooth'
    | 'performance'
    | 'ultra_low_latency'
    | 'graalvm'
    | 'legacy'
    | 'legacy_pvp'
    | 'legacy_heavy';
  performance_mode: PerformanceMode;
  guardian_mode: GuardianMode;
  guardian_idle_integrity_enabled: boolean;
  theme: '' | 'obsidian' | 'deepslate' | 'nether' | 'end' | 'birch' | 'custom';
  custom_hue: number | null;
  custom_vibrancy: number | null;
  lightness: number | null;
  onboarding_done: boolean;
  telemetry_enabled: boolean;
  discord_rpc_enabled: boolean;
  discord_rpc_onboarding_seen: boolean;
  music_enabled: boolean | null;
  music_volume: number | null;
  music_track: number;
}

export interface SystemInfo {
  total_memory_mb: number;
  recommended_min_mb: number;
  recommended_max_mb: number;
  max_allocatable_gb: number;
}
