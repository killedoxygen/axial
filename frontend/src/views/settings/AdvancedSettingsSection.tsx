import type { JSX } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { api } from '../../api';
import { hasNativeDesktopRuntime, requestNativeAppReset } from '../../native';
import { Button, Toggle } from '../../ui/Atoms';
import { SettingRow, SettingsSection } from '../../ui/SettingsSheet';
import { navigate } from '../../ui-state';
import { config, devMode } from '../../store';
import { toast } from '../../toast';
import { errMessage } from '../../utils';

type PerformanceLabCardComponent = (typeof import('./PerformanceLabCard'))['PerformanceLabCard'];

function PerformanceLabSlot(): JSX.Element | null {
  const isDev = devMode.value;
  const [Lab, setLab] = useState<PerformanceLabCardComponent | null>(null);

  useEffect(() => {
    if (!isDev) {
      setLab(null);
      return;
    }

    let alive = true;
    void import('./PerformanceLabCard').then((module) => {
      if (alive) setLab(() => module.PerformanceLabCard);
    });
    return () => {
      alive = false;
    };
  }, [isDev]);

  if (!isDev || !Lab) return null;
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
      const res: any = await api('PUT', '/config', { telemetry_enabled: next });
      if (res?.error) throw new Error(res.error);
      config.value = res;
      toast('Saved');
    } catch (err) {
      setTelemetryEnabled(savedTelemetry);
      toast(`Could not save anonymous usage stats setting: ${errMessage(err)}`, 'error');
    } finally {
      setSavingTelemetry(false);
    }
  };

  const resetLauncher = async (): Promise<void> => {
    if (resetInFlight.current) return;
    resetInFlight.current = true;
    try {
      const { showConfirm } = await import('../../ui/Dialog');
      const confirmed = await showConfirm(
        'Delete startup-detected Axial launcher files and the default managed library, then restart? External libraries and your saved Microsoft system credential are preserved.',
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
      toast('Reset complete. Restarting Axial.');
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
          <Button variant="secondary" icon="refresh" onClick={() => location.reload()}>
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
      {isDev && <PerformanceLabSlot />}
      {isDev && hasNativeDesktopRuntime() && (
        <SettingRow
          title="Reset launcher"
          description="Deletes startup-detected launcher files and the default managed library. External libraries and the saved Microsoft system credential are preserved."
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
