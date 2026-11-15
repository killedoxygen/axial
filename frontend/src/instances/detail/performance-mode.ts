import { config } from '../../settings/state';
import type { ConfigPerformanceMode } from '../../generated/ConfigPerformanceMode';

export function performanceModeFrom(value: string | undefined): ConfigPerformanceMode | null {
  if (value === 'managed' || value === 'vanilla' || value === 'custom') return value;
  return null;
}

export function globalPerformanceMode(): ConfigPerformanceMode {
  return performanceModeFrom(config.value?.performance_mode) ?? 'managed';
}

export function performanceModeLabel(mode: ConfigPerformanceMode): string {
  if (mode === 'managed') return 'Managed';
  if (mode === 'vanilla') return 'Vanilla';
  return 'Custom';
}

/** What each mode adds at launch; shared by the global and instance settings. */
export const performanceModeNotes: Record<ConfigPerformanceMode, string> = {
  managed: 'Axial chooses JVM settings for each Java version.',
  vanilla: "Pure Minecraft. Only the game's own launch settings and your memory.",
  custom: 'Only the JVM preset and arguments you choose.',
};
