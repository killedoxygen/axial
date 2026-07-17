import type { JSX } from 'preact';
import { Icon } from '../../../ui/Icons';
import { Button, Meter } from '../../../ui/Atoms';
import { DownloadFailureNotice } from '../../../ui/DownloadFailureNotice';
import { openContextMenu } from '../../../ui/ContextMenu';
import { navigate } from '../../../ui-state';
import { clearLaunchNotice } from '../../../actions';
import { toast } from '../../../toast';
import type { LaunchState } from '../../../store';
import type { DownloadFailure } from '../../../machines/downloads';
import type { InstanceInstallProgress } from '../../../instance-install-status';
import type { LaunchActionState, LaunchNotice } from '../../../types-launch';
import type { EnrichedInstance } from '../../../types-instance';
import type { InstallQueuedItemViewModel } from '../../../types-install';
import { launchActionPresentation, launchNoticePresentation } from '../../../launch-presenters';
import { openInstanceFolder } from '../instance-actions';

export function LaunchOutcomeNotice({ inst, notice }: { inst: EnrichedInstance; notice: LaunchNotice }): JSX.Element {
  const { icon, primaryDetail, listDetails } = launchNoticePresentation(notice);

  return (
    <div class="cp-instance-notice-shell">
      <section class="cp-notice" data-tone={notice.tone} aria-live="polite">
        <span class="cp-notice-mark" aria-hidden="true">
          <Icon name={icon} size={15} stroke={2.2} />
        </span>
        <div class="cp-notice-copy">
          <strong>{notice.message}</strong>
          {primaryDetail && <p>{primaryDetail}</p>}
          {listDetails.length > 0 && (
            <details class="cp-notice-details">
              <summary>Details</summary>
              <ul>
                {listDetails.map((detail, index) => (
                  <li key={`${index}:${detail}`}>{detail}</li>
                ))}
              </ul>
            </details>
          )}
        </div>
        <button
          class="cp-notice-dismiss"
          type="button"
          aria-label="Dismiss launch notice"
          onClick={() => clearLaunchNotice(inst.id)}
        >
          <Icon name="x" size={13} stroke={2.2} />
        </button>
      </section>
    </div>
  );
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
  launchAction: LaunchActionState;
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
  const primaryMenuItem = blocked
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
    <div
      class={`cp-instance-split-launch${progress ? ' cp-instance-split-launch--preparing' : ''}`}
      role="group"
      aria-label="Instance actions"
      style={{ '--cp-launch-pct': `${pct}%` } as any}
    >
      {progress?.determinate && <span class="cp-instance-split-launch-fill" aria-hidden="true" />}
      <button
        class="cp-instance-split-launch-main"
        type="button"
        onClick={disabled ? undefined : primaryAction}
        data-sound={usesInstallAction ? 'bright' : 'launchPress'}
        disabled={disabled}
      >
        <Icon name={icon} size={18} stroke={1.8} />
        <span>{label}</span>
      </button>
      <button
        class="cp-instance-split-launch-menu"
        type="button"
        aria-label="Instance options"
        aria-haspopup="menu"
        disabled={Boolean(progress)}
        onClick={(e) =>
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
        <Icon name="chevron-down" size={16} stroke={2.3} />
      </button>
      {progress?.determinate && <span class="cp-instance-launch-status">{Math.round(pct)}%</span>}
    </div>
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
      <div class="cp-instance-install-pane">
        <DownloadFailureNotice failure={installFailure} onRetry={onRetryInstall} trailing={<OpenDownloadsButton />} />
      </div>
    );
  }

  const pct = installProgress ? Math.max(0, Math.min(100, Math.round(installProgress.pct))) : 0;
  const targetLabel = installLabel || installTarget;
  const heading = installQueued ? installQueuedView?.title || installLabel : 'Installing required files';
  const body = installProgress
    ? `${installProgress.label} for ${targetLabel}.`
    : installQueued
      ? installQueuedView?.summary || `${targetLabel} is waiting in the download queue.`
      : `Axial is preparing the required version files for ${targetLabel}.`;
  const detail = installProgress ? `${pct}% complete` : installQueued ? installQueuedView?.detail || '' : '';

  return (
    <div class="cp-instance-install-pane" aria-live="polite">
      <div class="cp-instance-install-head">
        <span class="cp-instance-install-icon" aria-hidden="true">
          <Icon name={installQueued ? 'clock' : 'download'} size={18} stroke={2} />
        </span>
        <div class="cp-instance-install-copy">
          <h2>{heading}</h2>
          <p>{body}</p>
        </div>
        {installProgress && <span class="cp-instance-install-pct">{pct}%</span>}
      </div>
      <Meter value={pct} height={6} ariaLabel={`Install progress for ${targetLabel}`} />
      <div class="cp-instance-install-foot">
        <span>{detail}</span>
        <OpenDownloadsButton />
      </div>
    </div>
  );
}
