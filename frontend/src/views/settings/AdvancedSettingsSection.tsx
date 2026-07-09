import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { api } from '../../api';
import { ensureFlags, refreshFlags, setFlagOverride } from '../../flags';
import { Button, Toggle } from '../../ui/Atoms';
import { SettingRow, SettingsSection } from '../../ui/SettingsSheet';
import { navigate, ROUTE_STORAGE_KEY } from '../../ui-state';
import { STORAGE_KEY } from '../../state';
import { config, devMode, featureFlags, featureFlagsLoadState } from '../../store';
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

const FLAG_STAGE_NOTES = {
  experimental: 'Experimental — may change or break.',
  beta: 'Beta — may still change.',
} as const;

export function ExperimentalFlagRows(): JSX.Element | null {
  const allFlags = featureFlags.value;
  const loadState = featureFlagsLoadState.value;

  useEffect(() => {
    if (!featureFlags.value) void ensureFlags().catch(() => undefined);
  }, []);

  if (!allFlags) {
    const failed = loadState.status === 'error';
    return (
      <SettingRow
        title="Experimental flags"
        description={
          failed ? `Could not load feature flags: ${loadState.error || 'Unknown error'}` : 'Feature flags are loading.'
        }
        control={
          failed ? (
            <Button variant="secondary" icon="refresh" onClick={() => void refreshFlags().catch(() => undefined)}>
              Retry
            </Button>
          ) : undefined
        }
      />
    );
  }

  const flags = allFlags.filter((flag) => !flag.dev_only);
  if (flags.length === 0) return null;

  return (
    <>
      {flags.map((flag) => (
        <SettingRow
          key={flag.key}
          title={flag.title}
          description={`${flag.description} ${FLAG_STAGE_NOTES[flag.stage]}`}
          control={<Toggle on={flag.enabled} onChange={() => void setFlagOverride(flag.key, !flag.enabled)} />}
        />
      ))}
    </>
  );
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
      toast(`Could not save anonymous usage stats setting: ${errMessage(err)}`, 'error');
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
    <SettingsSection>
      <SettingRow
        title="Anonymous usage stats"
        description="Shares anonymous feature-usage and launch-outcome events to improve Croopor. No names, files, or personal data — see docs/TELEMETRY.md. Builds without a telemetry key never upload."
        control={<Toggle on={telemetryEnabled} onChange={() => void toggleTelemetry()} />}
      />
      <SettingRow
        title="Reload launcher"
        description="Useful if the launcher gets out of sync with the backend."
        control={
          <Button variant="secondary" icon="refresh" onClick={() => location.reload()}>
            Reload
          </Button>
        }
      />
      <ExperimentalFlagRows />
      {__CROOPOR_ENABLE_DEV_LAB__ && isDev && (
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
      {isDev && (
        <SettingRow
          title="Flush all data"
          description="Deletes every Croopor-managed file and restarts from first run. Existing libraries selected through 'Use existing' are preserved."
          control={
            <Button variant="danger" icon="trash" disabled={busy} onClick={flush}>
              {busy ? 'Flushing…' : 'Flush'}
            </Button>
          }
        />
      )}
    </SettingsSection>
  );
}
