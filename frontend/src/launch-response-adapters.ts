import { backendLaunchNotice } from './launch-notice-tracker';
import type { LaunchSessionOutcome, LaunchStatusUpdate, LaunchStatusViewModel } from './types-launch';

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
  if (
    typeof candidate.state_id !== 'string' ||
    typeof candidate.label !== 'string' ||
    typeof candidate.progress_pct !== 'number' ||
    !Number.isFinite(candidate.progress_pct) ||
    typeof candidate.terminal !== 'boolean' ||
    typeof candidate.playing !== 'boolean' ||
    typeof candidate.process_live !== 'boolean' ||
    typeof candidate.can_stop !== 'boolean'
  ) {
    return null;
  }
  return {
    state_id: candidate.state_id,
    label: candidate.label,
    progress_pct: Math.max(0, Math.min(100, candidate.progress_pct)),
    terminal: candidate.terminal,
    playing: candidate.playing,
    process_live: candidate.process_live,
    can_stop: candidate.can_stop,
  };
}

export function launchStatusUpdate(value: unknown, sessionId: string): LaunchStatusUpdate | null {
  if (!value || typeof value !== 'object' || typeof sessionId !== 'string' || !sessionId) return null;
  const candidate = value as {
    session_id?: unknown;
    revision?: unknown;
    view_model?: unknown;
    notice?: unknown;
    outcome?: unknown;
  };
  if (candidate.session_id !== sessionId || !('notice' in candidate) || !('outcome' in candidate)) return null;
  const revision = launchStatusRevision(value);
  const viewModel = launchStatusViewModel(candidate.view_model);
  if (revision == null || !viewModel) return null;

  const notice = candidate.notice == null ? null : backendLaunchNotice(candidate.notice);
  if (candidate.notice != null && !notice) return null;
  const outcome = candidate.outcome == null ? null : (launchSessionOutcome(candidate.outcome) ?? null);
  if (candidate.outcome != null && !outcome) return null;
  if (viewModel.terminal !== Boolean(outcome)) return null;

  return { revision, viewModel, notice, outcome };
}

function launchStatusRevision(value: unknown): number | null {
  if (!value || typeof value !== 'object') return null;
  const revision = (value as { revision?: unknown }).revision;
  return Number.isSafeInteger(revision) && (revision as number) >= 0 ? (revision as number) : null;
}
