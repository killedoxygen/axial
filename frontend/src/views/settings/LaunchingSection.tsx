import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Toggle } from '../../ui/Atoms';
import { SelectField } from '../../ui/Select';
import { SettingRow, SettingsSection } from '../../ui/SettingsSheet';
import { MemoryField, recommendedHeapRange } from '../../ui/MemoryField';
import { WindowField } from '../../ui/WindowField';
import { JavaPathField } from '../../ui/RuntimeFields';
import { useJvmPresets, jvmPresetSelectLabel, normalizeJvmPreset } from '../../hooks/use-jvm-presets';
import { useAutoSave } from '../../hooks/use-autosave';
import { api } from '../../api';
import { config, systemInfo } from '../../store';
import { fmtMem } from '../../utils';
import type { Config } from '../../types-settings';

export function LaunchingSection(): JSX.Element {
  const cfg = config.value;
  const sys = systemInfo.value;
  const totalGb = sys?.total_memory_mb ? Math.max(1, Math.floor(sys.total_memory_mb / 1024)) : 16;
  const [recMin, recMax] = recommendedHeapRange(totalGb);

  const { commit } = useAutoSave<Config & { error?: string }>({
    send: (patch) => api('PUT', '/config', patch),
    apply: (res) => {
      config.value = res;
    },
    errorLabel: 'settings',
  });

  const savedMaxGb = (cfg?.max_memory_mb ?? 4096) / 1024;
  const savedMinGb = (cfg?.min_memory_mb ?? 1024) / 1024;
  const [minGb, setMinGb] = useState(savedMinGb);
  const [maxGb, setMaxGb] = useState(savedMaxGb);

  const savedJavaPath = cfg?.java_path_override ?? '';
  const [javaPath, setJavaPath] = useState(savedJavaPath);

  const savedDiscord = cfg?.discord_rpc_enabled !== false;
  const [discordOn, setDiscordOn] = useState(savedDiscord);

  useEffect(() => {
    setMinGb(savedMinGb);
    setMaxGb(savedMaxGb);
    setJavaPath(savedJavaPath);
    setDiscordOn(savedDiscord);
  }, [savedMinGb, savedMaxGb, savedJavaPath, savedDiscord]);

  const width = cfg?.window_width && cfg.window_width > 0 ? cfg.window_width : 854;
  const height = cfg?.window_height && cfg.window_height > 0 ? cfg.window_height : 480;

  const { options: presetOptions, selectable } = useJvmPresets();
  const jvmPreset = normalizeJvmPreset(cfg?.jvm_preset, selectable);
  const selectedPreset =
    presetOptions.find((option) => option.id === jvmPreset) ?? presetOptions.find((option) => option.default) ?? null;

  return (
    <>
      <SettingsSection title="Launch defaults">
        <SettingRow
          title="Memory"
          description={`JVM heap for instances that don't set their own. Recommended ${fmtMem(recMin)} to ${fmtMem(recMax)}.`}
          aside={<span class="cp-sheet-note">{totalGb} GB installed</span>}
        >
          <MemoryField
            minGb={minGb}
            maxGb={maxGb}
            totalGb={totalGb}
            onChange={(low, high) => {
              setMinGb(low);
              setMaxGb(high);
            }}
            onCommit={(low, high) =>
              commit(
                {
                  min_memory_mb: Math.round(Math.min(low, high) * 1024),
                  max_memory_mb: Math.round(high * 1024),
                },
                {
                  label: 'memory defaults',
                  revert: () => {
                    setMinGb(savedMinGb);
                    setMaxGb(savedMaxGb);
                  },
                },
              )
            }
          />
        </SettingRow>
        <SettingRow title="Window" description="Game window size for instances that don't set their own.">
          <WindowField
            width={width}
            height={height}
            onCommit={(w, h) => commit({ window_width: w, window_height: h }, { label: 'window defaults' })}
          />
        </SettingRow>
        <SettingRow
          title="Runtime"
          description={selectedPreset?.disabled_reason ?? selectedPreset?.detail ?? 'Java runtime and JVM preset.'}
        >
          <div class="cp-settings-runtime">
            <label class="cp-ovr-field">
              <span>JVM preset</span>
              <SelectField<string>
                value={jvmPreset}
                ariaLabel="JVM preset"
                disabled={selectable.length === 0}
                placeholder="Loading"
                onChange={(next) => commit({ jvm_preset: next }, { label: 'runtime defaults' })}
                options={presetOptions.map((preset) => ({
                  value: preset.id,
                  label: jvmPresetSelectLabel(preset),
                  disabled: Boolean(preset.disabled_reason),
                }))}
              />
            </label>
            <JavaPathField
              value={javaPath}
              onChange={setJavaPath}
              onCommit={(next) => {
                if (next === savedJavaPath) return;
                commit(
                  { java_path_override: next },
                  { label: 'runtime defaults', revert: () => setJavaPath(savedJavaPath) },
                );
              }}
            />
          </div>
        </SettingRow>
      </SettingsSection>
      <SettingsSection title="Integrations">
        <SettingRow
          title="Discord activity"
          description="Shows Croopor and broad Minecraft status on your Discord profile."
          control={
            <Toggle
              on={discordOn}
              onChange={() => {
                const next = !discordOn;
                setDiscordOn(next);
                commit(
                  { discord_rpc_enabled: next, discord_rpc_onboarding_seen: true },
                  { label: 'Discord activity', revert: () => setDiscordOn(savedDiscord) },
                );
              }}
            />
          }
        />
      </SettingsSection>
    </>
  );
}
