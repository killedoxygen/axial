import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { setConfig } from '../../actions';
import { ChoicePills, type ChoicePillOption } from '../../ui/ChoicePills';
import { SettingRow, SettingsSection } from '../../ui/SettingsSheet';
import { saveConfigPatch, useAutoSave } from '../../hooks/use-autosave';
import { config } from '../../store';
import type { Config } from '../../types-settings';
import type { PerformanceMode } from '../../types-performance';

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
  const [performanceMode, setPerformanceMode] = useState<PerformanceMode>(savedPerformance);

  useEffect(() => {
    setPerformanceMode(savedPerformance);
  }, [savedPerformance]);

  const { commit, saving } = useAutoSave<Config & { error?: string }>({
    send: saveConfigPatch,
    apply: setConfig,
    errorLabel: 'performance settings',
  });

  const performanceNote = PERFORMANCE_OPTIONS.find((option) => option.value === performanceMode)?.note;

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
    </SettingsSection>
  );
}
