import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { setConfig, config, systemInfo } from './state';
import { Chip, Toggle } from '../ui/Atoms';
import { SelectField } from '../ui/Select';
import { SettingRow, SettingsSection } from '../ui/SettingsSheet';
import { MemoryField, recommendedHeapRange } from '../ui/MemoryField';
import { WindowField } from '../ui/WindowField';
import { JavaPathField } from '../launch/RuntimeFields';
import { useJvmPresets, normalizeJvmPreset } from '../launch/jvm-presets';
import { saveConfigPatch, useAutoSave } from './autosave';
import { fmtMem } from '../ui/format';
import type { ConfigView } from '../generated/ConfigView';

export function LaunchingSection(): JSX.Element {
  const cfg = config.value;
  const sys = systemInfo.value;
  const totalGb = sys?.total_memory_mb ? Math.max(1, Math.floor(sys.total_memory_mb / 1024)) : 16;
  const [recMin, recMax] = recommendedHeapRange(totalGb);

  const { commit } = useAutoSave<ConfigView & { error?: string }>({
    send: saveConfigPatch,
    apply: setConfig,
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
  }, [savedMinGb, savedMaxGb]);

  useEffect(() => {
    setJavaPath(savedJavaPath);
  }, [savedJavaPath]);

  useEffect(() => {
    setDiscordOn(savedDiscord);
  }, [savedDiscord]);

  const width = cfg?.window_width && cfg.window_width > 0 ? cfg.window_width : 854;
  const height = cfg?.window_height && cfg.window_height > 0 ? cfg.window_height : 480;

  const presetOptions = useJvmPresets();
  const jvmPreset = normalizeJvmPreset(cfg?.jvm_preset, presetOptions);
  const selectedPreset =
    presetOptions.find((option) => option.id === jvmPreset) ?? presetOptions.find((option) => option.default) ?? null;

  return (
    <SettingsSection>
      <SettingRow
        title="Memory"
        description={`JVM heap for instances that don't set their own. Recommended ${fmtMem(recMin)} to ${fmtMem(recMax)}.`}
      >
        <MemoryField
          minGb={minGb}
          maxGb={maxGb}
          totalGb={totalGb}
          facts={<Chip>{totalGb} GB installed</Chip>}
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
      <SettingRow title="Game window" description="Game window size for instances that don't set their own.">
        <WindowField
          width={width}
          height={height}
          onCommit={(w, h) => commit({ window_width: w, window_height: h }, { label: 'window defaults' })}
        />
      </SettingRow>
      <SettingRow title="Runtime" description={selectedPreset?.detail ?? 'Java runtime and JVM preset.'}>
        <div class="cp-runtime-grid">
          <label class="cp-ovr-field">
            <span>JVM preset</span>
            <SelectField<string>
              value={jvmPreset}
              ariaLabel="JVM preset"
              disabled={presetOptions.length === 0}
              placeholder="Loading"
              onChange={(next) => commit({ jvm_preset: next }, { label: 'runtime defaults' })}
              options={presetOptions.map((preset) => ({
                value: preset.id,
                label: preset.label,
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
                { label: 'runtime defaults', revert: () => setJavaPath(config.value?.java_path_override ?? '') },
              );
            }}
          />
        </div>
      </SettingRow>
      <SettingRow
        title="Discord activity"
        description="Shows Axial and broad Minecraft status on your Discord profile."
        control={
          <Toggle
            on={discordOn}
            ariaLabel="Discord activity"
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
  );
}
