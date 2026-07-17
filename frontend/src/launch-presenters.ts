import type { LaunchActionState, LaunchNotice, LaunchNoticeTone } from './types-launch';
import type { LaunchState } from './store';

function launchNoticeIcon(tone: LaunchNoticeTone): string {
  if (tone === 'success') return 'check-circle';
  if (tone === 'error') return 'alert';
  if (tone === 'warned') return 'alert';
  if (tone === 'intervened') return 'shield-check';
  return 'info';
}

export function launchNoticePresentation(notice: LaunchNotice): {
  icon: string;
  primaryDetail: string;
  listDetails: string[];
} {
  const details = (notice.details ?? []).map((detail) => detail.trim()).filter(Boolean);
  const primaryDetail = notice.detail?.trim() || (details.length === 1 ? details[0] : '');
  return {
    icon: launchNoticeIcon(notice.tone),
    primaryDetail,
    listDetails: details.length > 1 ? details.filter((detail) => !primaryDetail || detail !== primaryDetail) : [],
  };
}

interface LaunchActionPresentationInput {
  launchAction: LaunchActionState;
  installQueued: boolean;
  installQueuedView?: { title: string; summary: string };
  installProgress: { pct: number; label: string } | null;
  preparing: Extract<LaunchState, { status: 'preparing' }> | null;
}

interface LaunchActionPresentation {
  progress: { pct: number; label: string; determinate: boolean } | null;
  usesInstallAction: boolean;
  blocked: boolean;
  label: string;
  icon: string;
  pct: number;
  disabled: boolean;
}

export function launchActionPresentation({
  launchAction,
  installQueued,
  installQueuedView,
  installProgress,
  preparing,
}: LaunchActionPresentationInput): LaunchActionPresentation {
  const progress = preparing
    ? { pct: preparing.pct, label: preparing.label, determinate: preparing.determinate !== false }
    : installProgress
      ? { ...installProgress, determinate: true }
      : null;
  const usesInstallAction = launchAction.primary_action === 'install';
  const blocked = launchAction.primary_action === 'blocked';
  const label = progress?.label || (installQueued ? installQueuedView?.title || 'Queued' : launchAction.label);
  const icon = progress || installQueued ? 'clock' : blocked ? 'alert' : usesInstallAction ? 'download' : 'play';
  const pct = progress?.determinate ? progress.pct : 0;
  const disabled = Boolean(progress) || installQueued || blocked;
  return { progress, usesInstallAction, blocked, label, icon, pct, disabled };
}
