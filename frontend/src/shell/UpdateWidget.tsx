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
  hasVisibleUpdate,
  openUpdateAction,
  openUpdateNotes,
  restartBlockedByActivity,
  restartDesktopApp,
  startUpdateDownload,
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
          ? 'Update ready'
          : phase === 'restart-pending'
            ? 'Restart to finish'
            : phase === 'failed'
              ? "Update didn't install"
              : 'Update available';

  let sub = latest;
  let subTone: 'default' | 'error' = 'default';
  if (phase === 'downloading') {
    sub = flow.total_bytes
      ? `${formatBytes(flow.received_bytes)} of ${formatBytes(flow.total_bytes)}`
      : formatBytes(flow.received_bytes);
  } else if (phase === 'applying') {
    sub = 'Finishing up';
  } else if (phase === 'ready') {
    sub = restartBlocked ? 'Waiting for downloads and games to finish' : `${latest} · restart to install`;
  } else if (phase === 'restart-pending') {
    sub = 'Applied — takes effect on next launch';
  } else if (phase === 'failed' && flow.message) {
    sub = flow.message;
    subTone = 'error';
  }

  const busy = phase === 'downloading' || phase === 'applying';
  const indeterminate = flow.percent == null || phase === 'applying';

  const download = (): void => {
    void startUpdateDownload();
    if (!inApp) onClose();
  };

  return (
    <div class="cp-update-card cp-nodrag" role="dialog" aria-label="App update">
      <div class="cp-update-card-head">
        <span class="cp-update-card-title">{title}</span>
        {sub && (
          <span class="cp-update-card-sub" data-tone={subTone}>
            {sub}
          </span>
        )}
      </div>

      {busy && (
        <div
          class="cp-update-card-bar"
          data-indeterminate={indeterminate}
          role="progressbar"
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={flow.percent ?? undefined}
        >
          <span
            class="cp-update-card-bar-fill"
            style={!indeterminate && flow.percent != null ? { width: `${flow.percent}%` } : undefined}
          />
        </div>
      )}

      {!busy && (
        <div class="cp-update-card-actions">
          {(phase === 'idle' || phase === 'failed') && (
            <>
              {inApp ? (
                <Button variant="primary" size="sm" icon="download" onClick={download}>
                  {phase === 'failed' ? 'Try again' : 'Download'}
                </Button>
              ) : (
                <Button variant="primary" size="sm" icon="globe" onClick={() => void openUpdateAction()}>
                  {info?.action_label || 'Open release'}
                </Button>
              )}
              <Button variant="ghost" size="sm" onClick={() => void openUpdateNotes()}>
                Notes
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
            </>
          )}
          {phase === 'ready' && (
            <>
              <Button
                variant="primary"
                size="sm"
                icon="refresh"
                disabled={restartBlocked}
                onClick={() => void applyUpdateAndRestart()}
              >
                Restart to update
              </Button>
              <Button variant="ghost" size="sm" onClick={() => void openUpdateNotes()}>
                Notes
              </Button>
            </>
          )}
          {phase === 'restart-pending' && (
            <Button variant="primary" size="sm" icon="refresh" onClick={() => void restartDesktopApp()}>
              Restart now
            </Button>
          )}
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
  const pct = flow.percent;
  const ratio = pct != null ? Math.min(100, Math.max(0, pct)) / 100 : 0;
  const triggerStyle = { '--cp-update-ratio': String(ratio) } as JSX.CSSProperties;

  return (
    <div class="cp-update-dock-wrap cp-nodrag" ref={rootRef}>
      <button
        class="cp-update-dock"
        data-open={open}
        data-busy={busy}
        style={busy ? triggerStyle : undefined}
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
