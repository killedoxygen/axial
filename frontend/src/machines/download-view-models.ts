import type {
  InstallActionViewModel,
  InstallFailureViewModel,
  InstallProgressStepViewModel,
  InstallProgressViewModel,
  InstallQueueNoticeViewModel,
} from '../types-install';

export function installProgressStepViewModel(value: unknown): InstallProgressStepViewModel | null {
  if (!value || typeof value !== 'object') return null;
  const candidate = value as {
    phase_id?: unknown;
    label?: unknown;
    progress_pct?: unknown;
    current?: unknown;
    total?: unknown;
  };
  if (typeof candidate.phase_id !== 'string' || typeof candidate.label !== 'string') return null;
  const pct =
    typeof candidate.progress_pct === 'number' && Number.isFinite(candidate.progress_pct) ? candidate.progress_pct : 0;
  return {
    phase_id: candidate.phase_id,
    label: candidate.label,
    progress_pct: Math.max(0, Math.min(100, pct)),
    current:
      typeof candidate.current === 'number' && Number.isFinite(candidate.current) ? candidate.current : undefined,
    total: typeof candidate.total === 'number' && Number.isFinite(candidate.total) ? candidate.total : undefined,
  };
}

export function installProgressViewModel(value: unknown): InstallProgressViewModel | null {
  if (!value || typeof value !== 'object') return null;
  const candidate = value as Partial<InstallProgressViewModel>;
  if (typeof candidate.phase_id !== 'string' || typeof candidate.label !== 'string') return null;
  const pct =
    typeof candidate.progress_pct === 'number' && Number.isFinite(candidate.progress_pct) ? candidate.progress_pct : 0;
  return {
    phase_id: candidate.phase_id,
    label: candidate.label,
    progress_pct: Math.max(0, Math.min(100, pct)),
    terminal: candidate.terminal === true,
    failed: candidate.failed === true,
    active_step: installProgressStepViewModel(candidate.active_step),
  };
}

function installActionViewModel(value: unknown, fallback: InstallActionViewModel): InstallActionViewModel {
  if (!value || typeof value !== 'object') return fallback;
  const candidate = value as Partial<InstallActionViewModel>;
  if (typeof candidate.action !== 'string' || typeof candidate.label !== 'string') return fallback;
  return {
    action: candidate.action,
    label: candidate.label.trim() || fallback.label,
    enabled: candidate.enabled === true,
    disabled_reason:
      typeof candidate.disabled_reason === 'string' && candidate.disabled_reason.trim()
        ? candidate.disabled_reason.trim()
        : null,
  };
}

export function installFailureViewModel(value: unknown): InstallFailureViewModel | null {
  if (!value || typeof value !== 'object') return null;
  const candidate = value as Partial<InstallFailureViewModel>;
  if (
    typeof candidate.state_id !== 'string' ||
    typeof candidate.title !== 'string' ||
    typeof candidate.tone !== 'string' ||
    typeof candidate.summary !== 'string'
  ) {
    return null;
  }
  const retryFallback = unavailableFailureAction('retry', 'Retry unavailable');
  const dismissFallback = dismissFailureAction();
  const repairFallback = unavailableFailureAction('repair', 'Repair unavailable');
  return {
    state_id: candidate.state_id,
    title: candidate.title.trim() || 'Install failed',
    tone: candidate.tone.trim() || 'err',
    summary: candidate.summary.trim() || 'Install failed.',
    detail: typeof candidate.detail === 'string' && candidate.detail.trim() ? candidate.detail.trim() : null,
    details: Array.isArray(candidate.details)
      ? candidate.details.filter((detail): detail is string => typeof detail === 'string' && detail.trim().length > 0)
      : [],
    retry_action: installActionViewModel(candidate.retry_action, retryFallback),
    dismiss_action: installActionViewModel(candidate.dismiss_action, dismissFallback),
    repair_action: installActionViewModel(candidate.repair_action, repairFallback),
  };
}

function unavailableFailureAction(action: string, label: string): InstallActionViewModel {
  return {
    action,
    label,
    enabled: false,
    disabled_reason: 'Action unavailable until Croopor receives backend failure details.',
  };
}

function dismissFailureAction(): InstallActionViewModel {
  return {
    action: 'dismiss',
    label: 'Dismiss',
    enabled: true,
    disabled_reason: null,
  };
}

function boundedFailureMessage(message: string): string {
  const firstUsefulLine = String(message || '')
    .split(/\r?\n/)
    .map((line) => line.trim())
    .find((line) => line && !line.startsWith('at '));
  const squashed = (firstUsefulLine || 'Install failed before Croopor received error details.')
    .replace(/\s+/g, ' ')
    .trim();
  if (squashed.length <= 220) return squashed;
  return `${squashed.slice(0, 217).trimEnd()}...`;
}

export function unresolvedFailureViewModel(message: string): InstallFailureViewModel {
  const summary = boundedFailureMessage(message);
  return {
    state_id: 'failure_details_unavailable',
    title: 'Install failed',
    tone: 'err',
    summary,
    detail: null,
    details: [],
    retry_action: unavailableFailureAction('retry', 'Retry unavailable'),
    dismiss_action: dismissFailureAction(),
    repair_action: unavailableFailureAction('repair', 'Repair unavailable'),
  };
}

export function queueNoticeToastKind(notice: InstallQueueNoticeViewModel): 'success' | 'error' | 'info' {
  if (notice.tone === 'error' || notice.tone === 'err') return 'error';
  if (notice.tone === 'warn' || notice.tone === 'warning') return 'info';
  return notice.state_id === 'queued' || notice.state_id === 'retry_queued' ? 'success' : 'info';
}
