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
