import type { JSX } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { saveConfigPatch } from '../../hooks/use-autosave';
import { hasNativeDesktopRuntime, requestNativeAppReset } from '../../native';
import { Button, Toggle } from '../../ui/Atoms';
import { SettingRow, SettingsSection } from '../../ui/SettingsSheet';
import { navigate } from '../../ui-state';
import { config, devMode } from '../../store';
import { toast } from '../../toast';
import { errMessage } from '../../utils';
import { reloadApplication } from '../../preferences/persistence';

type PerformanceLabCardComponent = (typeof import('./PerformanceLabCard'))['PerformanceLabCard'];

const loadPerformanceLabCard = __AXIAL_ENABLE_DEV_LAB__
  ? async (): Promise<PerformanceLabCardComponent> => (await import('./PerformanceLabCard')).PerformanceLabCard
  : null;

function PerformanceLabSlot(): JSX.Element | null {
  const isDev = devMode.value;
  const [Lab, setLab] = useState<PerformanceLabCardComponent | null>(null);

  useEffect(() => {
    if (!isDev || !loadPerformanceLabCard) {
      setLab(null);
      return;
    }

    let alive = true;
    void loadPerformanceLabCard()
      .then((component) => {
        if (alive) setLab(() => component);
      })
      .catch((err: unknown) => {
        if (alive) toast(`Could not load Performance Lab: ${errMessage(err)}`, 'error');
      });
    return () => {
      alive = false;
    };
  }, [isDev]);

  if (!loadPerformanceLabCard || !isDev || !Lab) return null;
  return <Lab />;
}

export function AdvancedSettingsSection(): JSX.Element {
  const cfg = config.value;
  const isDev = devMode.value;
  const savedTelemetry = cfg?.telemetry_enabled === true;
  const [telemetryEnabled, setTelemetryEnabled] = useState(savedTelemetry);
  const [savingTelemetry, setSavingTelemetry] = useState(false);
  const [resetting, setResetting] = useState(false);
  const resetInFlight = useRef(false);

  useEffect(() => {
    setTelemetryEnabled(savedTelemetry);
  }, [savedTelemetry]);

  const toggleTelemetry = async (): Promise<void> => {
    if (savingTelemetry) return;
    const next = !telemetryEnabled;
    setTelemetryEnabled(next);
    setSavingTelemetry(true);
    try {
      await saveConfigPatch({ telemetry_enabled: next });
      toast('Saved');
    } catch (err) {
      setTelemetryEnabled(savedTelemetry);
      toast(`Could not save anonymous usage stats setting: ${errMessage(err)}`, 'error');
    } finally {
      setSavingTelemetry(false);
    }
  };

  const resetLauncher = async (): Promise<void> => {
    if (!devMode.value || !hasNativeDesktopRuntime()) return;
    if (resetInFlight.current) return;
    resetInFlight.current = true;
    try {
      const { showConfirm } = await import('../../ui/Dialog');
      const confirmed = await showConfirm(
        'Stop active work, delete this isolated Axial rewrite development profile and its managed library, then restart? The window will close while cleanup finishes. If cleanup is delayed, Axial will keep retrying before restarting. If the process stops before cleanup finishes, Axial will ask before continuing at the next startup. Other Axial profiles, external libraries and saved Microsoft system credentials are preserved.',
        {
          destructive: true,
          confirmText: 'Reset',
        },
      );
      if (!confirmed) {
        resetInFlight.current = false;
        return;
      }

      setResetting(true);
      const requested = await requestNativeAppReset();
      if (!requested) throw new Error('desktop runtime unavailable');
      toast('Reset requested. Axial will close, finish cleanup, then restart.');
    } catch (err) {
      resetInFlight.current = false;
      setResetting(false);
      toast(`Reset could not complete: ${errMessage(err)}`, 'error');
    }
  };

  return (
    <SettingsSection>
      <SettingRow
        title="Anonymous usage stats"
        description="Shares anonymous usage and launch stats to improve Axial. Never includes names, files, or personal data."
        control={<Toggle on={telemetryEnabled} onChange={() => void toggleTelemetry()} />}
      />
      <SettingRow
        title="Reload launcher"
        description="Restarts the interface if something looks stuck or out of date."
        control={
          <Button variant="secondary" icon="refresh" onClick={reloadApplication}>
            Reload
          </Button>
        }
      />
      {__AXIAL_ENABLE_DEV_LAB__ && isDev && (
        <SettingRow
          title="Dev lab"
          description="Developer workbench: feature flags, live state inspector, and UI playgrounds."
          control={
            <Button variant="secondary" icon="palette" onClick={() => navigate({ name: 'dev-lab' })}>
              Open lab
            </Button>
          }
        />
      )}
      {__AXIAL_ENABLE_DEV_LAB__ && isDev && <PerformanceLabSlot />}
      {isDev && hasNativeDesktopRuntime() && (
        <SettingRow
          title="Reset launcher"
          description="Stops active work and deletes this isolated rewrite development profile and its managed library. Other profiles, external libraries and saved Microsoft system credentials are preserved."
          control={
            <Button variant="danger" icon="trash" disabled={resetting} onClick={() => void resetLauncher()}>
              {resetting ? 'Resetting…' : 'Reset'}
            </Button>
          }
        />
      )}
    </SettingsSection>
  );
}
