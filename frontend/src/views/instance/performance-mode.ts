import { api } from '../../api';
import { config } from '../../store';
import type { PerformanceHealthResponse, PerformanceMode } from '../../types-performance';

export async function fetchPerformanceHealth(instanceId: string): Promise<PerformanceHealthResponse | null> {
  const params = new URLSearchParams({ instance_id: instanceId });
  const res = await api<PerformanceHealthResponse & { error?: string }>(
    'GET',
    `/performance/health?${params.toString()}`,
  );
  if (res?.error) throw new Error(res.error);
  return res?.health ? res : null;
}

export function performanceModeFrom(value: string | undefined): PerformanceMode | null {
  if (value === 'managed' || value === 'vanilla' || value === 'custom') return value;
  return null;
}

export function globalPerformanceMode(): PerformanceMode {
  return performanceModeFrom(config.value?.performance_mode) ?? 'managed';
}

export function performanceModeLabel(mode: PerformanceMode): string {
  if (mode === 'managed') return 'Managed';
  if (mode === 'vanilla') return 'Vanilla';
  return 'Custom';
}
