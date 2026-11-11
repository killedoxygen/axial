import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
import { Button, Chip } from '../../ui/Atoms';
import { Face } from '../../ui/Face';
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

function checkedLabel(raw: string | undefined): string | undefined {
  const stamp = Date.parse(raw || '');
  if (Number.isNaN(stamp)) return undefined;
  const at = new Date(stamp);
  const time = at.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
  const day = at.toDateString() === new Date().toDateString() ? 'today' : at.toLocaleDateString();
  return `Last checked ${day} at ${time}`;
}

async function openHomepage(): Promise<void> {
  try {
    await openExternalURL('https://github.com/killedoxygen/axial');
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
  const restartRequired = flowState.phase === 'failed' && flowState.can_restart;
  const flowStaged = flowState.phase === 'ready' || flowState.phase === 'restart-pending' || restartRequired;
  const latestVersion = flowState.version || info?.latest_version || appVersion.value;
  const releaseChannel = displayReleaseChannel(appVersion.value);
  const status = flowBusy
    ? flowState.phase === 'applying'
      ? `Installing ${displayReleaseVersion(latestVersion)}...`
      : `Downloading ${displayReleaseVersion(latestVersion)}...`
    : restartRequired
      ? updateRestartRequested.value
        ? 'Restarting Axial...'
        : 'Restart Axial before continuing'
      : flowStaged
        ? flowState.phase === 'restart-pending'
          ? updateRestartRequested.value
            ? `Update installed. Axial is restarting into ${displayReleaseVersion(latestVersion)}`
            : 'Update installed. Restart Axial when you are ready'
          : `${displayReleaseVersion(latestVersion)} is downloaded and ready to install`
        : checking
          ? 'Checking for updates...'
          : info
            ? info.available
              ? `${latestVersion.replace(/^[vV]/, '')} is available`
              : 'Axial is up to date'
            : 'Updates have not been checked yet';
  const visibleUpdate = hasVisibleUpdate() && !flowBusy && !flowStaged;
  const restartBlocked = restartBlockedByActivity();
  const restartRequested = updateRestartRequested.value;

  const dismiss = (): void => {
    dismissAvailableUpdate();
    setDismissedAt(Date.now());
  };

  return (
    <SettingsSection>
      <div class="cp-about">
        <Face eyes="open" size={48} />
        <div class="cp-about-name">
          <h2>Axial</h2>
          <span class="cp-chips">
            <Chip mono>{appVersion.value.replace(/^[vV]/, '')}</Chip>
            <Chip>{releaseChannel}</Chip>
          </span>
        </div>
      </div>
      <SettingRow title={status} description={checkedLabel(info?.checked_at)}>
        <div class="cp-setting-control">
          {flowState.phase === 'ready' && (
            <Button
              variant="primary"
              icon="refresh"
              disabled={restartBlocked}
              onClick={() => void applyUpdateAndRestart()}
            >
              Restart to update
            </Button>
          )}
          {flowStaged && flowState.phase !== 'ready' && (
            <Button
              variant="primary"
              icon="refresh"
              disabled={restartRequested || restartBlocked}
              onClick={() => void restartDesktopApp()}
            >
              {restartRequested ? 'Restarting…' : 'Restart now'}
            </Button>
          )}
          {visibleUpdate &&
            (canInstallUpdateInApp() ? (
              <Button
                variant="primary"
                icon="download"
                disabled={flowState.phase === 'failed' && !flowState.can_download}
                onClick={() => void startUpdateDownload()}
              >
                {flowState.phase === 'failed' ? 'Try again' : 'Download update'}
              </Button>
            ) : (
              <Button variant="primary" icon="globe" onClick={() => void openUpdateAction()}>
                {info?.action_label || 'Open release'}
              </Button>
            ))}
          {(flowStaged || visibleUpdate) && (
            <Button variant="secondary" onClick={() => void openUpdateNotes()}>
              Release notes
            </Button>
          )}
          {visibleUpdate && info?.checksum_url && (
            <Button variant="secondary" onClick={() => void openUpdateChecksum()}>
              Checksum
            </Button>
          )}
          <Button variant="secondary" disabled={checking} onClick={() => void checkForUpdates({ force: true })}>
            {checking ? 'Checking...' : 'Check'}
          </Button>
          {hasNativeDesktopRuntime() && (
            <Button variant="secondary" onClick={() => void restartDesktopApp()}>
              Restart
            </Button>
          )}
          {visibleUpdate && (
            <Button variant="ghost" onClick={dismiss}>
              Dismiss
            </Button>
          )}
        </div>
        {checkState === 'error' && !flowBusy && !flowStaged && (
          <p class="cp-about-line" data-tone="err">
            Could not check for updates.
          </p>
        )}
        {flowState.phase === 'failed' && flowState.message && (
          <p class="cp-about-line" data-tone="err">
            {flowState.message}
          </p>
        )}
        {flowBusy && (
          <p class="cp-about-line">
            {flowState.phase === 'applying'
              ? 'Installing...'
              : flowState.total_bytes
                ? `${flowState.percent ?? 0}%, ${formatBytes(flowState.received_bytes)} of ${formatBytes(flowState.total_bytes)}`
                : formatBytes(flowState.received_bytes)}
          </p>
        )}
        {flowState.phase === 'ready' && restartBlocked && (
          <p class="cp-about-line">Waiting for downloads and running games to finish.</p>
        )}
      </SettingRow>
      <div>
        <Button variant="secondary" onClick={() => void openHomepage()}>
          Homepage
        </Button>
      </div>
    </SettingsSection>
  );
}
