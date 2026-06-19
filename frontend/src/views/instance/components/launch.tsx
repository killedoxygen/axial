import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Icon } from '../../../ui/Icons';
import { Button } from '../../../ui/Atoms';
import { openContextMenu } from '../../../ui/ContextMenu';
import { navigate } from '../../../ui-state';
import { clearLaunchNotice } from '../../../actions';
import { toast } from '../../../toast';
import type { InstallFailure, LaunchState } from '../../../store';
import type { LaunchActionState, LaunchNotice, LaunchNoticeTone } from '../../../types-launch';
import type { EnrichedInstance } from '../../../types-instance';
import type { InstallQueuedItemViewModel } from '../../../types-install';
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
  installProgress: InstallBarrierProgress | null;
  installFailure: InstallFailure | null;
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
  const label =
    installFailure?.viewModel.summary ||
    installProgress?.label ||
    (installQueued ? installQueuedView?.summary || installLabel : 'Preparing install');
  const retryAction = installFailure?.viewModel.retry_action;
  const repairAction = installFailure?.viewModel.repair_action;
  const targetLabel = installLabel || installTarget;
  const remainingSeconds = installProgress
    ? countDownRemainingSeconds(installProgress.remainingSeconds, installProgress.remainingSecondsUpdatedAt, etaNow)
    : undefined;
  const activeEta = remainingSeconds ? `${formatRemainingTime(remainingSeconds)} left` : '';
  const detail = failed
    ? installFailure?.viewModel.detail || retryAction?.disabled_reason || 'Open Downloads for more context.'
    : installProgress
      ? activeEta
        ? `${activeEta} · ${pct}% complete`
        : `${pct}% complete`
      : installQueued
        ? installQueuedView?.detail || installQueuedView?.summary || ''
        : 'Croopor is preparing the required version files.';

  return (
    <div class="cp-instance-install-lock" aria-live="polite">
      <div class="cp-instance-install-lock-main">
        <span class="cp-instance-install-lock-icon" aria-hidden="true">
          <Icon name={failed ? 'alert' : installQueued ? 'clock' : 'download'} size={18} stroke={2} />
        </span>
        <div class="cp-instance-install-lock-copy">
          <h2>
            {failed
              ? installFailure?.viewModel.title || 'Install failed'
              : installQueued
                ? installQueuedView?.title || installLabel
                : 'Installing required files'}
          </h2>
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
            <>
              {repairAction && (
                <Button
                  variant="secondary"
                  size="sm"
                  icon="shield-check"
                  disabled={!repairAction.enabled}
                  title={repairAction.disabled_reason || undefined}
                >
                  {repairAction.label}
                </Button>
              )}
              <Button
                variant="secondary"
                size="sm"
                icon="refresh"
                onClick={onRetryInstall}
                disabled={retryAction ? !retryAction.enabled : false}
                title={retryAction?.disabled_reason || undefined}
              >
                {retryAction?.label || 'Retry install'}
              </Button>
            </>
          )}
          <Button variant="secondary" size="sm" icon="download" onClick={() => navigate({ name: 'downloads' })}>
            Downloads
          </Button>
        </div>
      </div>
    </div>
  );
}
