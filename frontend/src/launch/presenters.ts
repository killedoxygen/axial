import type { InstanceLaunchAction } from '../generated/InstanceLaunchAction';
import type { LaunchSession } from './state';
import type { LaunchState } from './state';
import type { IconName } from '../ui/Icons';

type LaunchSessionPresentationInput = Pick<LaunchSession, 'viewModel'>;

export function launchSessionIsPlaying(session: LaunchSessionPresentationInput | undefined): boolean {
  return session?.viewModel?.playing === true;
}

export function launchSessionCanStop(session: LaunchSessionPresentationInput | undefined): boolean {
  return session?.viewModel?.can_stop === true;
}

export function launchSessionHasLiveProcess(session: LaunchSessionPresentationInput | undefined): boolean {
  return session?.viewModel?.process_live === true;
}

export function launchSessionActivityLabel(session: LaunchSessionPresentationInput | undefined): string {
  const backendLabel = session?.viewModel?.label.trim();
  if (backendLabel) return backendLabel;
  return 'Preparing launch';
}

interface LaunchActionPresentationInput {
  launchAction: InstanceLaunchAction;
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
  icon: IconName;
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
      ? // Until a first measured step there is no figure to show: the ring travels instead of sitting at 0.
        { ...installProgress, determinate: installProgress.pct > 0 }
      : null;
  const usesInstallAction = launchAction.primary_action === 'install';
  const blocked = launchAction.primary_action === 'blocked';
  const label = progress?.label || (installQueued ? installQueuedView?.title || 'Queued' : launchAction.label);
  const icon: IconName =
    progress || installQueued ? 'clock' : blocked ? 'alert' : usesInstallAction ? 'download' : 'play';
  const pct = progress?.determinate ? progress.pct : 0;
  const disabled = Boolean(progress) || installQueued || blocked;
  return { progress, usesInstallAction, blocked, label, icon, pct, disabled };
}
