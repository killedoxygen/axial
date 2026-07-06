import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { api } from '../../api';
import { Button, Toggle } from '../../ui/Atoms';
import { navigate, ROUTE_STORAGE_KEY } from '../../ui-state';
import { STORAGE_KEY } from '../../state';
import { config, devMode } from '../../store';
import { toast } from '../../toast';
import { errMessage } from '../../utils';
import { SettingsCard } from './settings-shared';

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
  const [busy, setBusy] = useState(false);

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
      toast(`Could not save diagnostics setting: ${errMessage(err)}`, 'error');
    } finally {
      setSavingTelemetry(false);
    }
  };

  const flush = async (): Promise<void> => {
    const { showConfirm } = await import('../../ui/Dialog');
    const ok = await showConfirm('Delete all Croopor-owned data and reset the launcher to first run?', {
      destructive: true,
      confirmText: 'Reset',
    });
    if (!ok) return;
    setBusy(true);
    try {
      await api('POST', '/dev/flush');
      for (const key of [STORAGE_KEY, ROUTE_STORAGE_KEY]) {
        localStorage.removeItem(key);
      }
      location.reload();
    } catch (err) {
      toast(`Failed: ${errMessage(err)}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <SettingsCard
        title="Optional diagnostics"
        desc="Stores diagnostics consent. Current builds do not upload telemetry or open a remote diagnostics channel."
        control={<Toggle on={telemetryEnabled} onChange={() => void toggleTelemetry()} />}
      />
      <SettingsCard
        title="Reload launcher"
        desc="Useful if the launcher gets out of sync with the backend."
        control={
          <Button variant="secondary" icon="refresh" onClick={() => location.reload()}>
            Reload
          </Button>
        }
      />
      {__CROOPOR_ENABLE_DEV_LAB__ && isDev && (
        <SettingsCard
          title="Dev lab"
          desc="Developer-only workbench for procedural art and internal experiments."
          control={
            <Button variant="secondary" icon="palette" onClick={() => navigate({ name: 'dev-lab' })}>
              Open lab
            </Button>
          }
        />
      )}
      {isDev && <PerformanceLabSlot />}
      {isDev && (
        <SettingsCard
          title="Flush all data"
          desc="Deletes every Croopor-managed file and restarts from first run. Existing libraries selected through 'Use existing' are preserved."
          control={
            <Button variant="danger" icon="trash" disabled={busy} onClick={flush}>
              {busy ? 'Flushing…' : 'Flush'}
            </Button>
          }
        />
      )}
    </>
  );
}
