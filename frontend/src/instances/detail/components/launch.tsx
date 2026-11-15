import type { JSX } from 'preact';
import { Button, Meter, Notice, SplitButton } from '../../../ui/Atoms';
import { DownloadFailureNotice } from '../../../install/DownloadFailureNotice';
import { openContextMenu, type ContextMenuItem } from '../../../ui/ContextMenu';
import { navigate } from '../../../app/route';
import { clearLaunchNotice } from '../../../launch/state';
import { toast } from '../../../app/toast';
import type { LaunchState } from '../../../launch/state';
import type { DownloadFailure } from '../../../install/queue';
import type { InstanceInstallProgress } from '../../../install/status';
import type { InstanceLaunchAction } from '../../../generated/InstanceLaunchAction';
import type { SessionNotice } from '../../../generated/SessionNotice';
import type { EnrichedInstance } from '../../../generated/EnrichedInstance';
import type { InstallQueuedItemViewModel } from '../../../generated/InstallQueuedItemViewModel';
import { launchActionPresentation } from '../../../launch/presenters';
import { openInstanceFolder } from '../instance-actions';

export function LaunchOutcomeNotice({ inst, notice }: { inst: EnrichedInstance; notice: SessionNotice }): JSX.Element {
  return <Notice tone={notice.tone} title={notice.message} onDismiss={() => clearLaunchNotice(inst.id)} />;
}

export function LaunchSplitButton({
  inst,
  launchAction,
  installQueued,
  installQueuedView,
  installProgress,
  onLaunch,
  onInstall,
  onOpenLogs,
  onOpenSettings,
  preparing,
}: {
  inst: EnrichedInstance;
  launchAction: InstanceLaunchAction;
  installQueued: boolean;
  installQueuedView?: InstallQueuedItemViewModel;
  installProgress: { pct: number; label: string } | null;
  onLaunch: () => void;
  onInstall: () => void;
  onOpenLogs: () => void;
  onOpenSettings: () => void;
  preparing: Extract<LaunchState, { status: 'preparing' }> | null;
}): JSX.Element {
  const { progress, usesInstallAction, blocked, label, icon, pct, disabled } = launchActionPresentation({
    launchAction,
    installQueued,
    installQueuedView,
    installProgress,
    preparing,
  });
  const primaryAction = usesInstallAction ? onInstall : onLaunch;
  const primaryMenuItem: ContextMenuItem = blocked
    ? {
        icon: 'alert',
        label: launchAction.disabled_reason || launchAction.label,
        onSelect: () => toast(launchAction.disabled_reason || launchAction.label, 'error'),
      }
    : usesInstallAction
      ? {
          icon: installQueued ? 'clock' : 'download',
          label: installQueued ? installQueuedView?.title || 'Install queued' : launchAction.label,
          onSelect: installQueued
            ? () => {
                const message = installQueuedView?.summary || installQueuedView?.title || '';
                if (message) toast(message, 'info');
              }
            : onInstall,
        }
      : { icon: 'play', label: 'Launch now', onSelect: onLaunch };
  return (
    <SplitButton
      size="lg"
      label="Instance actions"
      menuLabel="Instance options"
      icon={icon}
      busy={Boolean(progress)}
      progress={progress?.determinate ? pct : null}
      // A launch being prepared reports a measured figure; an install's fill is an estimate, so it has no number.
      trailing={preparing && progress?.determinate && <span class="cp-btn-note">{Math.round(pct)}%</span>}
      disabled={disabled}
      sound={usesInstallAction ? 'bright' : 'launchPress'}
      onClick={primaryAction}
      onMenu={(e) =>
        openContextMenu(e, [
          primaryMenuItem,
          { icon: 'settings', label: 'Launch settings', onSelect: onOpenSettings },
          { icon: 'terminal', label: 'View launch logs', onSelect: onOpenLogs },
          { label: '', onSelect: () => {}, divider: true },
          { icon: 'folder', label: 'Open instance folder', onSelect: () => void openInstanceFolder(inst.id) },
          {
            icon: 'folder',
            label: 'Open resource packs folder',
            onSelect: () => void openInstanceFolder(inst.id, 'resourcepacks'),
          },
          {
            icon: 'folder',
            label: 'Open shader packs folder',
            onSelect: () => void openInstanceFolder(inst.id, 'shaderpacks'),
          },
        ])
      }
    >
      {label}
    </SplitButton>
  );
}

function OpenDownloadsButton(): JSX.Element {
  return (
    <Button variant="secondary" size="sm" icon="download" onClick={() => navigate({ name: 'downloads' })}>
      Downloads
    </Button>
  );
}

export function InstallBarrierPane({
  installTarget,
  installLabel,
  installQueued,
  installQueuedView,
  installProgress,
  installFailure,
  onRetryInstall,
}: {
  installTarget: string;
  installLabel: string;
  installQueued: boolean;
  installQueuedView?: InstallQueuedItemViewModel;
  installProgress: InstanceInstallProgress | null;
  installFailure: DownloadFailure | null;
  onRetryInstall: () => void;
}): JSX.Element {
  if (installFailure) {
    return (
      <section class="cp-install">
        <DownloadFailureNotice failure={installFailure} onRetry={onRetryInstall} trailing={<OpenDownloadsButton />} />
      </section>
    );
  }

  // The Launch button already names the step and how far it is; this says why the tabs are away, and shows it.
  const pct = installProgress ? Math.max(0, Math.min(100, Math.round(installProgress.pct))) : 0;
  const targetLabel = installLabel || installTarget;
  const heading = installQueued ? installQueuedView?.title || installLabel : 'Installing required files';
  const body = installProgress
    ? ''
    : installQueued
      ? installQueuedView?.summary || `${targetLabel} is waiting in the download queue.`
      : `Axial is preparing the required version files for ${targetLabel}.`;
  const detail = installQueued && !installProgress ? installQueuedView?.detail || '' : '';

  return (
    <section class="cp-install" aria-live="polite">
      <div class="cp-install-copy">
        <h2>{heading}</h2>
        {body && <p>{body}</p>}
      </div>
      <Meter value={pct} height={6} ariaLabel={`Install progress for ${targetLabel}`} />
      <div class="cp-install-foot">
        <span>{detail}</span>
        <OpenDownloadsButton />
      </div>
    </section>
  );
}
