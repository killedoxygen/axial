import type { JSX } from 'preact';
import { Icon } from '../../../ui/Icons';
import { Button, IconButton, Meter, Ring } from '../../../ui/Atoms';
import { Face, type Eyes, type FaceTone } from '../../../ui/Face';
import { DownloadFailureNotice } from '../../../ui/DownloadFailureNotice';
import { openContextMenu, type ContextMenuItem } from '../../../ui/ContextMenu';
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

const NOTICE_FACE: Record<LaunchNotice['tone'], { eyes: Eyes; tone?: FaceTone }> = {
  info: { eyes: 'open' },
  warned: { eyes: 'wide', tone: 'warn' },
  intervened: { eyes: 'happy' },
  success: { eyes: 'happy' },
  error: { eyes: 'x', tone: 'err' },
};

export function LaunchOutcomeNotice({ inst, notice }: { inst: EnrichedInstance; notice: LaunchNotice }): JSX.Element {
  const { primaryDetail, listDetails } = launchNoticePresentation(notice);
  return (
    <section class="cp-notice" data-tone={notice.tone} aria-live="polite">
      <Face {...NOTICE_FACE[notice.tone]} />
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
      <IconButton icon="x" size={28} tooltip="Dismiss launch notice" onClick={() => clearLaunchNotice(inst.id)} />
    </section>
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
    <div class="cp-launch" role="group" aria-label="Instance actions" data-busy={Boolean(progress)}>
      <button
        class="cp-btn cp-btn--lg cp-btn--primary"
        type="button"
        onClick={disabled ? undefined : primaryAction}
        data-sound={usesInstallAction ? 'bright' : 'launchPress'}
        disabled={disabled}
      >
        {progress ? <Ring progress={progress.determinate ? pct : null} /> : <Icon name={icon} size={16} />}
        <span>{label}</span>
        {progress?.determinate && <span class="cp-launch-pct">{Math.round(pct)}%</span>}
      </button>
      <button
        class="cp-btn cp-btn--lg cp-btn--primary cp-launch-menu"
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
        <Icon name="chevron-down" size={14} />
      </button>
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
      <section class="cp-install">
        <DownloadFailureNotice failure={installFailure} onRetry={onRetryInstall} trailing={<OpenDownloadsButton />} />
      </section>
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
    <section class="cp-install" aria-live="polite">
      <div class="cp-install-copy">
        <h2>{heading}</h2>
        <p>{body}</p>
      </div>
      <Meter value={pct} height={6} ariaLabel={`Install progress for ${targetLabel}`} />
      <div class="cp-install-foot">
        <span>{detail}</span>
        <OpenDownloadsButton />
      </div>
    </section>
  );
}
