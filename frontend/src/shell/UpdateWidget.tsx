import type { JSX } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { Button } from '../ui/Atoms';
import { Icon } from '../ui/Icons';
import { updateInfo } from '../store';
import type { UpdateFlowState } from '../types-update';
import {
  applyUpdateAndRestart,
  canInstallUpdateInApp,
  dismissAvailableUpdate,
  downloadAndInstallUpdate,
  hasVisibleUpdate,
  openUpdateAction,
  openUpdateNotes,
  restartBlockedByActivity,
  restartDesktopApp,
  updateFlow,
} from '../updater';
import { formatBytes } from '../utils';

function displayVersion(version: string): string {
  if (!version) return '';
  return version.startsWith('v') || version.startsWith('V') ? version : `v${version}`;
}

function triggerIcon(phase: UpdateFlowState['phase']): string {
  if (phase === 'downloading' || phase === 'applying') return 'download';
  if (phase === 'ready' || phase === 'restart-pending') return 'refresh';
  return 'arrow-up';
}

function triggerText(flow: UpdateFlowState): string {
  switch (flow.phase) {
    case 'downloading':
      return flow.percent != null ? `${flow.percent}%` : 'Updating';
    case 'applying':
      return 'Installing';
    case 'ready':
    case 'restart-pending':
      return 'Restart';
    default:
      return 'Update';
  }
}

function triggerLabel(flow: UpdateFlowState, latest: string): string {
  switch (flow.phase) {
    case 'downloading':
      return flow.percent != null ? `Downloading update, ${flow.percent}%` : 'Downloading update';
    case 'applying':
      return 'Installing update';
    case 'ready':
      return `Restart to update to ${latest}`;
    case 'restart-pending':
      return 'Restart to finish updating';
    default:
      return `Update ${latest} available`;
  }
}

function cardHeadIcon(phase: UpdateFlowState['phase']): string {
  if (phase === 'downloading' || phase === 'applying') return 'download';
  if (phase === 'ready' || phase === 'restart-pending') return 'refresh';
  if (phase === 'failed') return 'alert';
  return 'arrow-up';
}

function UpdateCard({ latest, onClose }: { latest: string; onClose: () => void }): JSX.Element {
  const info = updateInfo.value;
  const flow = updateFlow.value;
  const { phase } = flow;
  const inApp = canInstallUpdateInApp();
  const restartBlocked = restartBlockedByActivity();

  const title =
    phase === 'downloading'
      ? 'Downloading update'
      : phase === 'applying'
        ? 'Installing update'
        : phase === 'ready'
          ? restartBlocked
            ? 'Update ready'
            : 'Restart to install'
          : phase === 'restart-pending'
            ? 'Restart to finish'
            : phase === 'failed'
              ? "Update didn't install"
              : `Update ${latest}`;

  let sub = 'A newer version of Axial is available.';
  let subTone: 'default' | 'error' = 'default';
  if (phase === 'downloading') {
    sub = flow.total_bytes
      ? `${formatBytes(flow.received_bytes)} of ${formatBytes(flow.total_bytes)} · then restarts`
      : `${formatBytes(flow.received_bytes)} · then restarts`;
  } else if (phase === 'applying') {
    sub = 'Finishing up. Axial will restart.';
  } else if (phase === 'ready') {
    sub = restartBlocked ? 'Waiting for downloads and games to finish.' : `Axial will restart into ${latest}.`;
  } else if (phase === 'restart-pending') {
    sub = 'Applied. Takes effect on next launch.';
  } else if (phase === 'failed') {
    sub = flow.message || 'Something went wrong while installing.';
    subTone = 'error';
  }

  const busy = phase === 'downloading' || phase === 'applying';
  const indeterminate = flow.percent == null || phase === 'applying';
  const pct = flow.percent != null ? Math.round(Math.max(0, Math.min(100, flow.percent))) : null;

  return (
    <div class="cp-update-card cp-nodrag" role="dialog" aria-label="App update">
      <div class="cp-update-card-head">
        <span class="cp-update-card-badge" data-tone={subTone === 'error' ? 'error' : 'accent'}>
          <Icon name={cardHeadIcon(phase)} size={15} stroke={2.2} />
        </span>
        <div class="cp-update-card-heading">
          <div class="cp-update-card-title">{title}</div>
          <div class="cp-update-card-sub" data-tone={subTone}>
            {sub}
          </div>
        </div>
      </div>

      {busy && (
        <div class="cp-update-progress">
          <div
            class="cp-boot-bar"
            role="progressbar"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={pct ?? undefined}
          >
            <div
              class="cp-boot-bar-fill"
              data-indeterminate={indeterminate}
              style={!indeterminate && pct != null ? { width: `${pct}%` } : undefined}
            />
          </div>
          {!indeterminate && pct != null && <span class="cp-update-progress-pct">{pct}%</span>}
        </div>
      )}

      {(phase === 'idle' || phase === 'failed') && (
        <>
          <div class="cp-update-card-actions">
            {inApp ? (
              <Button
                variant="primary"
                size="sm"
                icon="refresh"
                style={{ width: '100%' }}
                onClick={() => void downloadAndInstallUpdate()}
              >
                {phase === 'failed' ? 'Try again' : 'Update & restart'}
              </Button>
            ) : (
              <Button
                variant="primary"
                size="sm"
                icon="globe"
                style={{ width: '100%' }}
                onClick={() => void openUpdateAction()}
              >
                {info?.action_label || 'Open release'}
              </Button>
            )}
          </div>
          <div class="cp-update-card-links">
            <Button variant="ghost" size="sm" onClick={() => void openUpdateNotes()}>
              Release notes
            </Button>
            <Button
              variant="ghost"
              size="sm"
              style={{ marginLeft: 'auto' }}
              onClick={() => {
                dismissAvailableUpdate();
                onClose();
              }}
            >
              Skip
            </Button>
          </div>
        </>
      )}

      {phase === 'ready' && (
        <div class="cp-update-card-actions">
          <Button
            variant="primary"
            size="sm"
            icon="refresh"
            disabled={restartBlocked}
            style={{ width: '100%' }}
            onClick={() => void applyUpdateAndRestart()}
          >
            Restart now
          </Button>
        </div>
      )}

      {phase === 'restart-pending' && (
        <div class="cp-update-card-actions">
          <Button
            variant="primary"
            size="sm"
            icon="refresh"
            style={{ width: '100%' }}
            onClick={() => void restartDesktopApp()}
          >
            Restart now
          </Button>
        </div>
      )}
    </div>
  );
}

export function UpdateWidget(): JSX.Element | null {
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  const flow = updateFlow.value;

  useEffect(() => {
    if (!open) return undefined;
    const onClick = (e: MouseEvent): void => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === 'Escape') setOpen(false);
    };
    document.addEventListener('mousedown', onClick);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onClick);
      document.removeEventListener('keydown', onKey);
    };
  }, [open]);

  const busy = flow.phase === 'downloading' || flow.phase === 'applying';
  const staged = flow.phase === 'ready' || flow.phase === 'restart-pending';
  if (!busy && !staged && !hasVisibleUpdate()) return null;

  const latest = displayVersion(flow.version || updateInfo.value?.latest_version || '');
  const label = triggerLabel(flow, latest);
  const icon = triggerIcon(flow.phase);
  const text = triggerText(flow);

  return (
    <div class="cp-update-dock-wrap cp-nodrag" ref={rootRef}>
      <button
        class="cp-update-dock"
        data-open={open}
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-label={label}
        title={label}
        onClick={() => setOpen((o) => !o)}
      >
        <span class="cp-update-dock-icon" key={icon}>
          <Icon name={icon} size={15} stroke={2.2} />
        </span>
        <span class="cp-update-dock-label">{text}</span>
      </button>
      {open && <UpdateCard latest={latest} onClose={() => setOpen(false)} />}
    </div>
  );
}
