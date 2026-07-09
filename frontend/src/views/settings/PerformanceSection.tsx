import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { OptionList, type OptionListItem } from '../../ui/OptionList';
import { SettingRow, SettingsSection } from '../../ui/SettingsSheet';
import { useAutoSave } from '../../hooks/use-autosave';
import { api } from '../../api';
import { config } from '../../store';
import type { Config } from '../../types-settings';
import type { GuardianMode } from '../../types-guardian';
import type { PerformanceMode } from '../../types-performance';
import { PerformanceRulesStatusBlock, usePerformanceRulesStatus } from './PerformanceRulesStatus';

const PERFORMANCE_OPTIONS: Array<OptionListItem<PerformanceMode>> = [
  {
    value: 'managed',
    label: 'Managed',
    icon: 'sparkles',
    note: 'Croopor applies recommended tuning and optimizations for you.',
  },
  {
    value: 'vanilla',
    label: 'Vanilla',
    icon: 'cube',
    note: 'Pure Minecraft. No tweaks or add-ons applied at launch.',
  },
  {
    value: 'custom',
    label: 'Custom',
    icon: 'sliders',
    note: 'You set the tuning. Your manual choices are kept as-is.',
  },
];

const GUARDIAN_OPTIONS: Array<OptionListItem<GuardianMode>> = [
  {
    value: 'managed',
    label: 'Managed',
    icon: 'shield-check',
    note: 'Catches risky launch settings and fixes them automatically.',
  },
  {
    value: 'custom',
    label: 'Custom',
    icon: 'shield-person',
    note: 'Keeps your choices, warns instead of changing, blocks only fatal setups.',
  },
];

function performanceModeFrom(value: string | undefined): PerformanceMode {
  if (value === 'vanilla' || value === 'custom') return value;
  return 'managed';
}

function guardianModeFrom(value: string | undefined): GuardianMode {
  return value === 'custom' ? 'custom' : 'managed';
}

export function PerformanceSection(): JSX.Element {
  const cfg = config.value;
  const savedPerformance = performanceModeFrom(cfg?.performance_mode);
  const savedGuardian = guardianModeFrom(cfg?.guardian_mode);
  const [performanceMode, setPerformanceMode] = useState<PerformanceMode>(savedPerformance);
  const [guardianMode, setGuardianMode] = useState<GuardianMode>(savedGuardian);
  const rulesStatus = usePerformanceRulesStatus();

  useEffect(() => {
    setPerformanceMode(savedPerformance);
    setGuardianMode(savedGuardian);
  }, [savedPerformance, savedGuardian]);

  const { commit, saving } = useAutoSave<Config & { error?: string }>({
    send: (patch) => api('PUT', '/config', patch),
    apply: (res) => {
      config.value = res;
    },
    errorLabel: 'performance settings',
  });

  return (
    <SettingsSection>
      <SettingRow
        title="Performance mode"
        description="How launches are tuned by default. Instances can override this per profile."
      >
        <OptionList<PerformanceMode>
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
      </SettingRow>
      <SettingRow title="Guardian" description="The safety net around risky launch configurations.">
        <OptionList<GuardianMode>
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
      </SettingRow>
      <SettingRow title="Managed rules" description="The rule set powering managed tuning and its readiness.">
        <PerformanceRulesStatusBlock state={rulesStatus} />
      </SettingRow>
    </SettingsSection>
  );
}
