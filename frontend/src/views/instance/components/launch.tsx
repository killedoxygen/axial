import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Icon } from '../../../ui/Icons';
import { Button } from '../../../ui/Atoms';
import { openContextMenu } from '../../../ui/ContextMenu';
import { navigate } from '../../../ui-state';
import { clearLaunchNotice } from '../../../actions';
import { toast } from '../../../toast';
import type { InstallFailure, LaunchState } from '../../../store';
import type { EnrichedInstance, LaunchActionState, LaunchNotice, LaunchNoticeTone } from '../../../types';
import { countDownRemainingSeconds, formatRemainingTime } from '../../../progress-estimation';
import { openInstanceFolder } from '../instance-actions';

type InstallBarrierProgress = {
  pct: number;
  label: string;
  displayName?: string;
  remainingSeconds?: number;
  remainingSecondsUpdatedAt?: number;
};

function launchNoticeIcon(tone: LaunchNoticeTone): string {
  if (tone === 'success') return 'check-circle';
  if (tone === 'error') return 'alert';
  if (tone === 'warned') return 'alert';
  if (tone === 'intervened') return 'shield-check';
  return 'info';
}

export function LaunchOutcomeNotice({ inst, notice }: { inst: EnrichedInstance; notice: LaunchNotice }): JSX.Element {
  const details = (notice.details ?? []).map((detail) => detail.trim()).filter(Boolean);
  const primaryDetail = notice.detail?.trim() || (details.length === 1 ? details[0] : '');
  const listDetails = details.length > 1 ? details.filter((detail) => !primaryDetail || detail !== primaryDetail) : [];

  return (
    <div class="cp-instance-notice-shell">
      <section class="cp-launch-notice" data-tone={notice.tone} aria-live="polite">
        <span class="cp-launch-notice-mark" aria-hidden="true">
          <Icon name={launchNoticeIcon(notice.tone)} size={15} stroke={2.2} />
        </span>
        <div class="cp-launch-notice-copy">
          <strong>{notice.message}</strong>
          {primaryDetail && <p>{primaryDetail}</p>}
          {listDetails.length > 0 && (
            <details class="cp-launch-notice-details">
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
          class="cp-launch-notice-dismiss"
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
  installProgress: { pct: number; label: string } | null;
  onLaunch: () => void;
  onInstall: () => void;
  onOpenLogs: () => void;
  onOpenSettings: () => void;
  preparing: Extract<LaunchState, { status: 'preparing' }> | null;
}): JSX.Element {
  const progress = preparing ? { pct: preparing.pct, label: preparing.label } : installProgress;
  const usesInstallAction = launchAction.primary_action === 'install';
  const label = progress?.label || (installQueued ? 'Queued' : launchAction.label);
  const icon = progress || installQueued ? 'clock' : usesInstallAction ? 'download' : 'play';
  const pct = progress?.pct ?? 0;
  const disabled = Boolean(progress) || installQueued;
  const primaryAction = usesInstallAction ? onInstall : onLaunch;
  const primaryMenuItem = usesInstallAction
    ? {
        icon: installQueued ? 'clock' : 'download',
        label: installQueued ? 'Queued' : launchAction.label,
        onSelect: installQueued ? () => toast('Install already queued') : onInstall,
      }
    : { icon: 'play', label: 'Launch now', onSelect: onLaunch };
  return (
    <div
      class={`cp-instance-split-launch${progress ? ' cp-instance-split-launch--preparing' : ''}`}
      role="group"
      aria-label="Instance actions"
      style={{ '--cp-launch-pct': `${pct}%` } as any}
    >
      {progress && <span class="cp-instance-split-launch-fill" aria-hidden="true" />}
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
      {progress && <span class="cp-instance-launch-status">{Math.round(pct)}%</span>}
    </div>
  );
}

export function InstallBarrierPane({
  installTarget,
  installLabel,
  installQueued,
  installProgress,
  installFailure,
  installQueuePosition,
  installQueueCount,
  onRetryInstall,
}: {
  installTarget: string;
  installLabel: string;
  installQueued: boolean;
  installProgress: InstallBarrierProgress | null;
  installFailure: InstallFailure | null;
  installQueuePosition?: number;
  installQueueCount?: number;
  onRetryInstall: () => void;
}): JSX.Element {
  const [etaNow, setEtaNow] = useState(() => Date.now());
  const hasActiveEta = Boolean(installProgress?.remainingSeconds);

  useEffect(() => {
    if (!hasActiveEta) return;
    setEtaNow(Date.now());
    const intervalId = window.setInterval(() => {
      setEtaNow(Date.now());
    }, 1000);
    return () => {
      window.clearInterval(intervalId);
    };
  }, [hasActiveEta, installProgress?.remainingSeconds, installProgress?.remainingSecondsUpdatedAt]);

  const pct = installProgress ? Math.max(0, Math.min(100, Math.round(installProgress.pct))) : 0;
  const failed = Boolean(installFailure);
  const queuedBehind = installQueuePosition != null ? installQueuePosition - 1 : undefined;
  const queuedDetail =
    installQueuePosition != null && installQueueCount != null
      ? installQueuePosition === 1
        ? `Position 1 of ${installQueueCount}; next to start when the download slot opens.`
        : `Position ${installQueuePosition} of ${installQueueCount}; waiting behind ${queuedBehind} item${queuedBehind === 1 ? '' : 's'}.`
      : 'This instance will unlock automatically after its version install starts and finishes.';
  const label =
    installFailure?.message ||
    installProgress?.label ||
    (installQueued ? 'Install waiting in queue' : 'Preparing install');
  const targetLabel = installLabel || installTarget;
  const remainingSeconds = installProgress
    ? countDownRemainingSeconds(installProgress.remainingSeconds, installProgress.remainingSecondsUpdatedAt, etaNow)
    : undefined;
  const activeEta = remainingSeconds ? `${formatRemainingTime(remainingSeconds)} left` : '';
  const detail = failed
    ? 'Retry the required install or open Downloads for more context.'
    : installProgress
      ? activeEta
        ? `${activeEta} · ${pct}% complete`
        : `${pct}% complete`
      : installQueued
        ? queuedDetail
        : 'Croopor is preparing the required version files.';

  return (
    <div class="cp-instance-install-lock" aria-live="polite">
      <div class="cp-instance-install-lock-main">
        <span class="cp-instance-install-lock-icon" aria-hidden="true">
          <Icon name={failed ? 'alert' : installQueued ? 'clock' : 'download'} size={18} stroke={2} />
        </span>
        <div class="cp-instance-install-lock-copy">
          <h2>{failed ? 'Install failed' : installQueued ? 'Install queued' : 'Installing required files'}</h2>
          {failed ? (
            <>
              <p>Could not install {targetLabel}.</p>
              <p>{label}</p>
            </>
          ) : (
            <p>
              {label} for {targetLabel}.
            </p>
          )}
        </div>
      </div>

      <div class="cp-instance-install-lock-progress" style={{ '--cp-install-lock-pct': `${pct}%` } as any}>
        <span aria-hidden="true" />
      </div>

      <div class="cp-instance-install-lock-foot">
        <span>{detail}</span>
        <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginLeft: 'auto' }}>
          {failed && (
            <Button variant="secondary" size="sm" icon="refresh" onClick={onRetryInstall}>
              Retry
            </Button>
          )}
          <Button variant="secondary" size="sm" icon="download" onClick={() => navigate({ name: 'downloads' })}>
            Downloads
          </Button>
        </div>
      </div>
    </div>
  );
}
