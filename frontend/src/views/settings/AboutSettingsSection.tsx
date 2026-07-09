import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
import { Button } from '../../ui/Atoms';
import { hasNativeDesktopRuntime, openExternalURL } from '../../native';
import { appVersion, updateCheckState, updateInfo } from '../../store';
import { toast } from '../../toast';
import { errMessage } from '../../utils';
import {
  checkForUpdates,
  dismissAvailableUpdate,
  formatUpdateCheckTime,
  hasVisibleUpdate,
  openUpdateAction,
  openUpdateChecksum,
  openUpdateNotes,
  restartDesktopApp,
} from '../../updater';
import { SettingRow, SettingsSection } from '../../ui/SettingsSheet';

function displayReleaseVersion(version: string): string {
  return version.startsWith('v') || version.startsWith('V') ? version : `v${version}`;
}

async function openHomepage(): Promise<void> {
  try {
    await openExternalURL('https://github.com/mateoltd/croopor');
    toast('Opened homepage');
  } catch (err: unknown) {
    toast(`Failed to open homepage: ${errMessage(err)}`, 'error');
  }
}

export function AboutSettingsSection(): JSX.Element {
  const info = updateInfo.value;
  const checkState = updateCheckState.value;
  const [, setDismissedAt] = useState(0);
  const checking = checkState === 'checking';
  const latestVersion = info?.latest_version || appVersion.value;
  const status = checking
    ? 'Checking for updates...'
    : info
      ? info.available
        ? `Latest release: ${displayReleaseVersion(latestVersion)}`
        : `Current release: ${displayReleaseVersion(info.current_version)}`
      : 'Updates have not been checked yet.';
  const visibleUpdate = hasVisibleUpdate();
  const checkedAt = info ? formatUpdateCheckTime(info.checked_at) : 'Not checked yet';

  const dismiss = (): void => {
    dismissAvailableUpdate();
    setDismissedAt(Date.now());
  };

  return (
    <SettingsSection>
      <SettingRow title="Croopor" description={`Version ${appVersion.value}. A focused Minecraft launcher.`}>
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
        {checkState === 'error' && (
          <div style={{ marginTop: 8, color: 'var(--err)', fontSize: 12 }}>Could not check for updates.</div>
        )}
        {visibleUpdate && (
          <div style={{ marginTop: 12, display: 'flex', gap: 8, flexWrap: 'wrap' }}>
            <Button variant="primary" icon="globe" onClick={() => void openUpdateAction()}>
              {info?.action_label || 'Open release'}
            </Button>
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
