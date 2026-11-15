import type { PerformanceHealthResponse } from '../generated/PerformanceHealthResponse';
import type { PerformancePlanSummaryViewModel } from '../generated/PerformancePlanSummaryViewModel';

/** The managed bundle's health as a notice, when it needs attention. */
export function performanceHealthNotice(
  health: PerformanceHealthResponse | null,
): PerformancePlanSummaryViewModel | null {
  const view = health?.view_model;
  return view && (view.tone === 'warn' || view.tone === 'err') ? view : null;
}
