import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
import { Button } from '../../ui/Atoms';
import { hasNativeDesktopRuntime, openExternalURL } from '../../native';
import { appVersion, updateCheckState, updateInfo } from '../../store';
import { toast } from '../../toast';
import { errMessage } from '../../utils';
import { formatBytes } from '../../format';
import {
  applyUpdateAndRestart,
  canInstallUpdateInApp,
  checkForUpdates,
  dismissAvailableUpdate,
  formatUpdateCheckTime,
  hasVisibleUpdate,
  openUpdateAction,
  openUpdateChecksum,
  openUpdateNotes,
  restartBlockedByActivity,
  restartDesktopApp,
  startUpdateDownload,
  updateFlow,
  updateRestartRequested,
} from '../../updater';
import { SettingRow, SettingsSection } from '../../ui/SettingsSheet';

function displayReleaseVersion(version: string): string {
  return version.startsWith('v') || version.startsWith('V') ? version : `v${version}`;
}

function displayReleaseChannel(version: string): string {
  const prerelease = version.replace(/^[vV]/, '').split('-', 2)[1]?.split('.', 1)[0];
  switch (prerelease) {
    case 'dev':
      return 'Development';
    case 'alpha':
      return 'Alpha';
    case 'beta':
      return 'Beta';
    case 'rc':
      return 'Release candidate';
    default:
      return prerelease ? 'Prerelease' : 'Stable';
  }
}

async function openHomepage(): Promise<void> {
  try {
    await openExternalURL('https://github.com/mateoltd/axial');
    toast('Opened homepage');
  } catch (err: unknown) {
    toast(`Failed to open homepage: ${errMessage(err)}`, 'error');
  }
}

export function AboutSettingsSection(): JSX.Element {
  const info = updateInfo.value;
  const flowState = updateFlow.value;
  const checkState = updateCheckState.value;
  const [, setDismissedAt] = useState(0);
  const checking = checkState === 'checking';
  const flowBusy = flowState.phase === 'downloading' || flowState.phase === 'applying';
  const flowStaged = flowState.phase === 'ready' || flowState.phase === 'restart-pending';
  const latestVersion = flowState.version || info?.latest_version || appVersion.value;
  const releaseChannel = displayReleaseChannel(appVersion.value);
  const status = flowBusy
    ? flowState.phase === 'applying'
      ? `Installing ${displayReleaseVersion(latestVersion)}...`
      : `Downloading ${displayReleaseVersion(latestVersion)}...`
    : flowStaged
      ? flowState.phase === 'restart-pending'
        ? updateRestartRequested.value
          ? `Update installed. Axial is restarting into ${displayReleaseVersion(latestVersion)}.`
          : `Update installed. Restart Axial when you are ready.`
        : `${displayReleaseVersion(latestVersion)} is downloaded and ready to install.`
      : checking
        ? 'Checking for updates...'
        : info
          ? info.available
            ? `${displayReleaseChannel(latestVersion)} update available: ${displayReleaseVersion(info.current_version)} → ${displayReleaseVersion(latestVersion)}`
            : `Current release: ${displayReleaseVersion(info.current_version)}`
          : 'Updates have not been checked yet.';
  const visibleUpdate = hasVisibleUpdate() && !flowBusy && !flowStaged;
  const checkedAt = info ? formatUpdateCheckTime(info.checked_at) : 'Not checked yet';
  const restartBlocked = restartBlockedByActivity();
  const restartRequested = updateRestartRequested.value;

  const dismiss = (): void => {
    dismissAvailableUpdate();
    setDismissedAt(Date.now());
  };

  return (
    <SettingsSection>
      <SettingRow title="Axial" description={`Version ${appVersion.value}. A focused Minecraft launcher.`}>
        <div style={{ color: 'var(--text-mute)', fontSize: 12 }}>Channel: {releaseChannel}</div>
        <div style={{ marginTop: 12, display: 'flex', gap: 8, flexWrap: 'wrap' }}>
          <Button variant="secondary" icon="globe" onClick={() => void openHomepage()}>
            Homepage
          </Button>
          <Button
            variant="secondary"
            icon="refresh"
            disabled={checking}
            onClick={() => void checkForUpdates({ force: true })}
          >
            {checking ? 'Checking...' : 'Check'}
          </Button>
          {hasNativeDesktopRuntime() && (
            <Button variant="secondary" icon="refresh" onClick={() => void restartDesktopApp()}>
              Restart
            </Button>
          )}
        </div>
        <div style={{ marginTop: 12, color: 'var(--text)', fontSize: 13, fontWeight: 700 }}>{status}</div>
        <div style={{ marginTop: 4, color: 'var(--text-mute)', fontSize: 12 }}>Last checked: {checkedAt}</div>
        {checkState === 'error' && !flowBusy && !flowStaged && (
          <div style={{ marginTop: 8, color: 'var(--err)', fontSize: 12 }}>Could not check for updates.</div>
        )}
        {flowState.phase === 'failed' && flowState.message && (
          <div style={{ marginTop: 8, color: 'var(--err)', fontSize: 12 }}>{flowState.message}</div>
        )}
        {flowBusy && (
          <div style={{ marginTop: 8, color: 'var(--text-mute)', fontSize: 12, fontVariantNumeric: 'tabular-nums' }}>
            {flowState.phase === 'applying'
              ? 'Installing...'
              : flowState.total_bytes
                ? `${flowState.percent ?? 0}%, ${formatBytes(flowState.received_bytes)} of ${formatBytes(flowState.total_bytes)}`
                : formatBytes(flowState.received_bytes)}
          </div>
        )}
        {flowStaged && (
          <div style={{ marginTop: 12, display: 'flex', gap: 8, flexWrap: 'wrap' }}>
            {flowState.phase === 'ready' ? (
              <Button
                variant="primary"
                icon="refresh"
                disabled={restartBlocked}
                onClick={() => void applyUpdateAndRestart()}
              >
                Restart to update
              </Button>
            ) : (
              <Button
                variant="primary"
                icon="refresh"
                disabled={restartRequested}
                onClick={() => void restartDesktopApp()}
              >
                {restartRequested ? 'Restarting…' : 'Restart now'}
              </Button>
            )}
            <Button variant="secondary" icon="tag" onClick={() => void openUpdateNotes()}>
              Notes
            </Button>
          </div>
        )}
        {flowState.phase === 'ready' && restartBlocked && (
          <div style={{ marginTop: 8, color: 'var(--text-mute)', fontSize: 12 }}>
            Waiting for downloads and running games to finish.
          </div>
        )}
        {visibleUpdate && (
          <div style={{ marginTop: 12, display: 'flex', gap: 8, flexWrap: 'wrap' }}>
            {canInstallUpdateInApp() ? (
              <Button variant="primary" icon="download" onClick={() => void startUpdateDownload()}>
                {flowState.phase === 'failed' ? 'Try again' : 'Download update'}
              </Button>
            ) : (
              <Button variant="primary" icon="globe" onClick={() => void openUpdateAction()}>
                {info?.action_label || 'Open release'}
              </Button>
            )}
            <Button variant="secondary" icon="tag" onClick={() => void openUpdateNotes()}>
              Notes
            </Button>
            {info?.checksum_url && (
              <Button variant="secondary" icon="shield-check" onClick={() => void openUpdateChecksum()}>
                Checksum
              </Button>
            )}
            <Button variant="secondary" icon="x" onClick={dismiss}>
              Dismiss
            </Button>
          </div>
        )}
      </SettingRow>
    </SettingsSection>
  );
}
