import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Toggle } from '../../ui/Atoms';
import { ChoicePills, type ChoicePillOption } from '../../ui/ChoicePills';
import { SettingRow, SettingsSection } from '../../ui/SettingsSheet';
import { useAutoSave } from '../../hooks/use-autosave';
import { api } from '../../api';
import { config } from '../../store';
import type { Config } from '../../types-settings';
import type { GuardianMode } from '../../types-guardian';
import type { PerformanceMode } from '../../types-performance';
import { GUARDIAN_OPTIONS, guardianModeFrom } from '../../guardian-settings';

const PERFORMANCE_OPTIONS: Array<ChoicePillOption<PerformanceMode>> = [
  { value: 'managed', label: 'Managed', note: 'Axial applies recommended tuning and optimizations for you.' },
  { value: 'vanilla', label: 'Vanilla', note: 'Pure Minecraft. No tweaks or add-ons applied at launch.' },
  { value: 'custom', label: 'Custom', note: 'You set the tuning. Your manual choices are kept as-is.' },
];

function performanceModeFrom(value: string | undefined): PerformanceMode {
  if (value === 'vanilla' || value === 'custom') return value;
  return 'managed';
}

export function PerformanceSection(): JSX.Element {
  const cfg = config.value;
  const savedPerformance = performanceModeFrom(cfg?.performance_mode);
  const savedGuardian = guardianModeFrom(cfg?.guardian_mode);
  const savedIdleIntegrity = cfg?.guardian_idle_integrity_enabled ?? true;
  const [performanceMode, setPerformanceMode] = useState<PerformanceMode>(savedPerformance);
  const [guardianMode, setGuardianMode] = useState<GuardianMode>(savedGuardian);
  const [idleIntegrityEnabled, setIdleIntegrityEnabled] = useState(savedIdleIntegrity);

  useEffect(() => {
    setPerformanceMode(savedPerformance);
    setGuardianMode(savedGuardian);
    setIdleIntegrityEnabled(savedIdleIntegrity);
  }, [savedPerformance, savedGuardian, savedIdleIntegrity]);

  const { commit, saving } = useAutoSave<Config & { error?: string }>({
    send: (patch) => api('PUT', '/config', patch),
    apply: (res) => {
      config.value = res;
    },
    errorLabel: 'performance settings',
  });

  const performanceNote = PERFORMANCE_OPTIONS.find((option) => option.value === performanceMode)?.note;
  const guardianNote = GUARDIAN_OPTIONS.find((option) => option.value === guardianMode)?.note;

  return (
    <SettingsSection>
      <SettingRow
        title="Performance mode"
        description={performanceNote}
        control={
          <ChoicePills<PerformanceMode>
            value={performanceMode}
            options={PERFORMANCE_OPTIONS}
            disabled={saving}
            ariaLabel="Performance mode"
            onChange={(next) => {
              setPerformanceMode(next);
              commit(
                { performance_mode: next },
                { label: 'performance settings', revert: () => setPerformanceMode(savedPerformance) },
              );
            }}
          />
        }
      />
      <SettingRow
        title="Guardian"
        description={guardianNote}
        control={
          <ChoicePills<GuardianMode>
            value={guardianMode}
            options={GUARDIAN_OPTIONS}
            disabled={saving}
            ariaLabel="Guardian mode"
            onChange={(next) => {
              setGuardianMode(next);
              commit(
                { guardian_mode: next },
                { label: 'Guardian settings', revert: () => setGuardianMode(savedGuardian) },
              );
            }}
          />
        }
      />
      <SettingRow
        title="Idle integrity checks"
        description="When Guardian is Managed, verifies managed instance files while Axial is idle."
        control={
          <Toggle
            on={idleIntegrityEnabled}
            disabled={saving}
            ariaLabel="Idle integrity checks"
            onChange={() => {
              const next = !idleIntegrityEnabled;
              setIdleIntegrityEnabled(next);
              commit(
                { guardian_idle_integrity_enabled: next },
                {
                  label: 'idle integrity checks',
                  revert: () => setIdleIntegrityEnabled(savedIdleIntegrity),
                },
              );
            }}
          />
        }
      />
    </SettingsSection>
  );
}
