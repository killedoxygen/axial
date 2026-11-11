import type { JSX } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { Button, Meter, Ring } from '../ui/Atoms';
import { Face } from '../ui/Face';
import { Icon, type IconName } from '../ui/Icons';
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
  updateRestartRequested,
} from '../updater';
import { formatBytes } from '../format';

function displayVersion(version: string): string {
  if (!version) return '';
  return version.startsWith('v') || version.startsWith('V') ? version : `v${version}`;
}

function triggerIcon(phase: UpdateFlowState['phase']): IconName {
  if (phase === 'downloading' || phase === 'applying') return 'download';
  if (phase === 'ready' || phase === 'restart-pending') return 'refresh';
  return 'arrow-up';
}

function triggerText(flow: UpdateFlowState): string {
  if (updateRestartRequested.value) return 'Restarting';
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
  if (updateRestartRequested.value) return 'Update installed, restarting Axial';
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

/** The card names the bare version; the dock's label keeps the `v`. */
function bareVersion(version: string): string {
  return version.replace(/^v/i, '');
}

function UpdateCard({ latest, onClose }: { latest: string; onClose: () => void }): JSX.Element {
  const info = updateInfo.value;
  const flow = updateFlow.value;
  const { phase } = flow;
  const inApp = canInstallUpdateInApp();
  const restartBlocked = restartBlockedByActivity();
  const restartRequested = updateRestartRequested.value;
  const restartRequired = phase === 'failed' && flow.can_restart;
  const version = bareVersion(latest);
  const pct = flow.percent != null ? Math.round(Math.max(0, Math.min(100, flow.percent))) : null;

  let head: JSX.Element;
  if (phase === 'downloading' || phase === 'applying' || restartRequested) {
    const title = restartRequested
      ? 'Restarting Axial'
      : phase === 'applying'
        ? `Installing ${version}`
        : `Downloading ${version}`;
    head = (
      <div class="cp-update-card-head">
        <Ring />
        <strong>{title}</strong>
      </div>
    );
  } else {
    const failed = phase === 'failed';
    const staged = phase === 'ready' || phase === 'restart-pending';
    const title = failed ? "Update didn't install" : staged ? `${version} is ready` : `Axial ${version}`;
    const sub = failed
      ? flow.message || 'Something went wrong while installing.'
      : staged
        ? restartBlocked
          ? 'Waiting for downloads and games to finish.'
          : null
        : info?.current_version
          ? `You have ${info.current_version}`
          : null;
    head = (
      <div class="cp-update-card-head">
        <Face size={32} eyes={failed ? 'x' : staged ? 'happy' : 'open'} tone={failed ? 'err' : undefined} />
        <div class="cp-update-card-id">
          <strong>{title}</strong>
          {sub && <span data-tone={failed ? 'error' : undefined}>{sub}</span>}
        </div>
      </div>
    );
  }

  const bytes =
    phase === 'downloading'
      ? flow.total_bytes
        ? `${formatBytes(flow.received_bytes)} of ${formatBytes(flow.total_bytes)}`
        : formatBytes(flow.received_bytes)
      : phase === 'applying'
        ? 'Finishing up. Axial will restart.'
        : restartRequested
          ? `${version} will open shortly.`
          : null;

  return (
    <div class="cp-update-card cp-pop cp-nodrag" role="dialog" aria-label="App update">
      {head}

      {bytes != null && (
        <div class="cp-update-progress">
          {phase === 'downloading' && pct != null && <Meter value={pct} ariaLabel="Update download progress" />}
          <div class="cp-update-progress-line">
            <span>{bytes}</span>
            {phase === 'downloading' && pct != null && <span>{pct}%</span>}
          </div>
        </div>
      )}

      {(phase === 'idle' || (phase === 'failed' && !restartRequired)) && (
        <>
          {inApp ? (
            <Button
              variant="primary"
              icon="refresh"
              full
              disabled={phase === 'failed' && !flow.can_download}
              onClick={() => void downloadAndInstallUpdate()}
            >
              {phase === 'failed' ? 'Try again' : 'Update and restart'}
            </Button>
          ) : (
            <Button variant="primary" icon="globe" full onClick={() => void openUpdateAction()}>
              {info?.action_label || 'Open release'}
            </Button>
          )}
          <div class="cp-update-card-links">
            <Button variant="ghost" size="sm" onClick={() => void openUpdateNotes()}>
              Release notes
            </Button>
            <Button
              variant="ghost"
              size="sm"
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
        <Button
          variant="primary"
          icon="refresh"
          full
          disabled={restartBlocked}
          onClick={() => void applyUpdateAndRestart()}
        >
          Restart now
        </Button>
      )}

      {(phase === 'restart-pending' || restartRequired) && !restartRequested && (
        <Button
          variant="primary"
          icon="refresh"
          full
          disabled={restartBlocked}
          onClick={() => void restartDesktopApp()}
        >
          Restart now
        </Button>
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
