import type { LaunchSessionOutcome, LaunchStatusViewModel } from './types-launch';

const LAUNCH_SESSION_EXIT_REASONS = new Set<LaunchSessionOutcome['reason']>([
  'clean_exit',
  'external_user_closed',
  'launcher_stopped',
  'spawn_failed',
  'startup_failed',
  'startup_stalled',
  'watchdog_killed',
  'crashed_before_boot',
  'crashed_after_boot',
  'unknown_exit',
]);

function isLaunchSessionExitReason(value: unknown): value is LaunchSessionOutcome['reason'] {
  return typeof value === 'string' && LAUNCH_SESSION_EXIT_REASONS.has(value as LaunchSessionOutcome['reason']);
}

export function launchSessionOutcome(value: unknown): LaunchSessionOutcome | undefined {
  if (!value || typeof value !== 'object') return undefined;
  const candidate = value as Partial<LaunchSessionOutcome>;
  if (
    candidate.kind !== 'clean' &&
    candidate.kind !== 'stopped' &&
    candidate.kind !== 'failed' &&
    candidate.kind !== 'unknown'
  ) {
    return undefined;
  }
  if (!isLaunchSessionExitReason(candidate.reason) || typeof candidate.summary !== 'string') return undefined;
  return {
    reason: candidate.reason,
    kind: candidate.kind,
    summary: candidate.summary,
  };
}

export function launchStatusViewModel(value: unknown): LaunchStatusViewModel | null {
  if (!value || typeof value !== 'object') return null;
  const candidate = value as Partial<LaunchStatusViewModel>;
  if (typeof candidate.state_id !== 'string' || typeof candidate.label !== 'string') return null;
  const pct =
    typeof candidate.progress_pct === 'number' && Number.isFinite(candidate.progress_pct) ? candidate.progress_pct : 0;
  return {
    state_id: candidate.state_id,
    label: candidate.label,
    progress_pct: Math.max(0, Math.min(100, pct)),
    terminal: candidate.terminal === true,
  };
}
