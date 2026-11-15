import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { setConfig, config } from './state';
import { OptionTiles, SettingRow, SettingsSection, type OptionTile } from '../ui/SettingsSheet';
import { saveConfigPatch, useAutoSave } from './autosave';
import type { ConfigView } from '../generated/ConfigView';
import type { ConfigPerformanceMode } from '../generated/ConfigPerformanceMode';
import { performanceModeNotes } from '../instances/detail/performance-mode';

const PERFORMANCE_OPTIONS: Array<OptionTile<ConfigPerformanceMode>> = [
  { value: 'managed', label: 'Managed', note: performanceModeNotes.managed },
  { value: 'vanilla', label: 'Vanilla', note: performanceModeNotes.vanilla },
  { value: 'custom', label: 'Custom', note: performanceModeNotes.custom },
];

function performanceModeFrom(value: string | undefined): ConfigPerformanceMode {
  if (value === 'vanilla' || value === 'custom') return value;
  return 'managed';
}

export function PerformanceSection(): JSX.Element {
  const cfg = config.value;
  const savedPerformance = performanceModeFrom(cfg?.performance_mode);
  const [performanceMode, setPerformanceMode] = useState<ConfigPerformanceMode>(savedPerformance);

  useEffect(() => {
    setPerformanceMode(savedPerformance);
  }, [savedPerformance]);

  const { commit, saving } = useAutoSave<ConfigView>({
    send: saveConfigPatch,
    apply: setConfig,
    errorLabel: 'performance settings',
  });

  return (
    <SettingsSection>
      <SettingRow title="Mode">
        <OptionTiles<ConfigPerformanceMode>
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
    </SettingsSection>
  );
}
