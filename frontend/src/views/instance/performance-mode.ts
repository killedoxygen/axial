import { api } from '../../api';
import { config } from '../../store';
import type { PerformanceHealthResponse, PerformanceMode } from '../../types-performance';

export interface PerformanceHealthNotice {
  tone: 'warned' | 'error';
  title: string;
  detail: string;
}

export async function fetchPerformanceHealth(instanceId: string): Promise<PerformanceHealthResponse | null> {
  const params = new URLSearchParams({ instance_id: instanceId });
  const res = await api<PerformanceHealthResponse & { error?: string }>(
    'GET',
    `/performance/health?${params.toString()}`,
  );
  if (res?.error) throw new Error(res.error);
  return res?.health ? res : null;
}

export function performanceHealthNotice(health: PerformanceHealthResponse | null): PerformanceHealthNotice | null {
  const viewModel = health?.view_model;
  if (!viewModel || (viewModel.tone !== 'warn' && viewModel.tone !== 'err')) return null;
  return {
    tone: viewModel.tone === 'warn' ? 'warned' : 'error',
    title: viewModel.title,
    detail: viewModel.detail,
  };
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
