import type { JSX } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { Icon } from '../../../ui/Icons';
import { SelectField } from '../../../ui/Select';
import { ChoicePills, type ChoicePillOption } from '../../../ui/ChoicePills';
import { OverrideChip, SettingRow, SettingsSection } from '../../../ui/SettingsSheet';
import { MemoryField, recommendedHeapRange } from '../../../ui/MemoryField';
import { WindowField } from '../../../ui/WindowField';
import { JavaPathField, JvmArgsInput } from '../../../ui/RuntimeFields';
import { useAutoSave } from '../../../hooks/use-autosave';
import { jvmPresetSelectLabel, normalizeJvmPreset, useJvmPresets } from '../../../hooks/use-jvm-presets';
import { api } from '../../../api';
import { config, systemInfo } from '../../../store';
import { updateInstanceInList } from '../../../actions';
import { fmtMem } from '../../../utils';
import type { InstancePerformanceMode } from '../../../types-performance';
import type { EnrichedInstance } from '../../../types-instance';
import { memoryGb } from '../format';
import {
  fetchPerformanceHealth,
  globalPerformanceMode,
  performanceModeFrom,
  performanceModeLabel,
} from '../performance-mode';

function instancePerformanceModeFrom(value: string | undefined): InstancePerformanceMode {
  return performanceModeFrom(value) ?? '';
}

function windowDimension(value: number | undefined, fallback: number): number {
  return Number.isFinite(value) && (value ?? 0) > 0 ? value! : fallback;
}

export function SettingsPane({ inst }: { inst: EnrichedInstance }): JSX.Element {
  const cfg = config.value;
  const globalMode = globalPerformanceMode();
  const totalGb = systemInfo.value?.total_memory_mb
    ? Math.max(1, Math.floor(systemInfo.value.total_memory_mb / 1024))
    : 32;
  const [recMin, recMax] = recommendedHeapRange(totalGb);

  const { commit, saving } = useAutoSave<EnrichedInstance & { error?: string }>({
    send: (patch) => api('PUT', `/instances/${encodeURIComponent(inst.id)}`, patch),
    apply: (res) => updateInstanceInList(res),
    errorLabel: 'instance settings',
  });

  const [healthRefreshKey, setHealthRefreshKey] = useState(0);
  const bumpHealth = (): void => setHealthRefreshKey((current) => current + 1);
  const [healthNotice, setHealthNotice] = useState<{
    tone: 'warned' | 'error';
    title: string;
    detail: string;
  } | null>(null);

  const memoryOverridden = (inst.max_memory_mb ?? 0) > 0 || (inst.min_memory_mb ?? 0) > 0;
  const savedMaxGb = memoryGb(inst.max_memory_mb, cfg?.max_memory_mb ?? 4096);
  const savedMinGb = memoryGb(inst.min_memory_mb, cfg?.min_memory_mb ?? 1024);
  const [maxGb, setMaxGb] = useState(savedMaxGb);
  const [minGb, setMinGb] = useState(savedMinGb);

  const windowOverridden = (inst.window_width ?? 0) > 0 || (inst.window_height ?? 0) > 0;
  const globalWidth = windowDimension(cfg?.window_width, 854);
  const globalHeight = windowDimension(cfg?.window_height, 480);
  const effectiveWidth = windowDimension(inst.window_width, globalWidth);
  const effectiveHeight = windowDimension(inst.window_height, globalHeight);

  const savedMode = instancePerformanceModeFrom(inst.performance_mode);
  const [mode, setMode] = useState<InstancePerformanceMode>(savedMode);

  const { options: presetOptions, selectable: selectablePresets } = useJvmPresets();
  const savedPreset = normalizeJvmPreset(inst.jvm_preset, selectablePresets);
  const selectedPreset =
    presetOptions.find((option) => option.id === savedPreset) ?? presetOptions.find((option) => option.default) ?? null;

  const savedJavaPath = inst.java_path ?? '';
  const [javaPath, setJavaPath] = useState(savedJavaPath);
  const runtimeOverridden = savedPreset !== '' || savedJavaPath.trim() !== '';

  const savedArgs = inst.extra_jvm_args ?? '';
  const [jvmArgs, setJvmArgs] = useState(savedArgs);
  const argsTimer = useRef<number | null>(null);
  const pendingArgs = useRef<string | null>(null);

  useEffect(() => {
    setMaxGb(savedMaxGb);
    setMinGb(savedMinGb);
    setMode(savedMode);
    setJavaPath(savedJavaPath);
    setJvmArgs(savedArgs);
  }, [inst.id, savedMaxGb, savedMinGb, savedMode, savedJavaPath, savedArgs]);

  useEffect(() => {
    let cancelled = false;
    void api('GET', `/instances/${encodeURIComponent(inst.id)}`)
      .then((res: any) => {
        if (cancelled || !res || res.error) return;
        updateInstanceInList(res as EnrichedInstance);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [inst.id]);

  useEffect(() => {
    let cancelled = false;
    void fetchPerformanceHealth(inst.id)
      .then((health) => {
        if (cancelled) return;
        const viewModel = health?.view_model;
        if (viewModel && (viewModel.tone === 'warn' || viewModel.tone === 'err')) {
          setHealthNotice({
            tone: viewModel.tone === 'warn' ? 'warned' : 'error',
            title: viewModel.title,
            detail: viewModel.detail,
          });
        } else {
          setHealthNotice(null);
        }
      })
      .catch(() => {
        if (!cancelled) setHealthNotice(null);
      });
    return () => {
      cancelled = true;
    };
  }, [inst.id, inst.performance_mode, globalMode, healthRefreshKey]);

  const commitArgs = (next: string): void => {
    pendingArgs.current = null;
    if (next === savedArgs) return;
    commit(
      { extra_jvm_args: next },
      { label: 'JVM arguments', revert: () => setJvmArgs(savedArgs), onSuccess: bumpHealth },
    );
  };

  const onArgsChange = (next: string): void => {
    setJvmArgs(next);
    pendingArgs.current = next;
    if (argsTimer.current !== null) window.clearTimeout(argsTimer.current);
    argsTimer.current = window.setTimeout(() => {
      argsTimer.current = null;
      if (pendingArgs.current !== null) commitArgs(pendingArgs.current);
    }, 600);
  };

  useEffect(() => {
    return () => {
      if (argsTimer.current !== null) {
        window.clearTimeout(argsTimer.current);
        argsTimer.current = null;
      }
      if (pendingArgs.current !== null) commitArgs(pendingArgs.current);
    };
  }, [inst.id]);

  const modeOptions: Array<ChoicePillOption<InstancePerformanceMode>> = [
    {
      value: '',
      label: 'Inherit',
      note: `Follows the global Performance setting, currently ${performanceModeLabel(globalMode)}.`,
    },
    {
      value: 'managed',
      label: 'Managed',
      note: 'Croopor applies recommended tuning and optimizations for this instance.',
    },
    { value: 'vanilla', label: 'Vanilla', note: 'Pure Minecraft. No tweaks or add-ons applied at launch.' },
    { value: 'custom', label: 'Custom', note: 'You set the tuning. Your manual choices are kept as-is.' },
  ];

  const changeMode = (next: InstancePerformanceMode): void => {
    setMode(next);
    commit(
      { performance_mode: next },
      { label: 'launch profile', revert: () => setMode(savedMode), onSuccess: bumpHealth },
    );
  };

  return (
    <div class="cp-instance-body cp-settings-pane">
      <div class="cp-resource-toolbar cp-settings-toolbar">
        <strong>Instance settings</strong>
        {saving && (
          <span class="cp-settings-saving" aria-live="polite">
            Saving…
          </span>
        )}
      </div>

      {healthNotice && (
        <section class="cp-notice" data-tone={healthNotice.tone} aria-live="polite">
          <span class="cp-notice-mark" aria-hidden="true">
            <Icon name="alert" size={15} stroke={2.2} />
          </span>
          <div class="cp-notice-copy">
            <strong>{healthNotice.title}</strong>
            {healthNotice.detail && <p>{healthNotice.detail}</p>}
          </div>
        </section>
      )}

      <SettingsSection>
        <SettingRow
          title="Launch profile"
          description={modeOptions.find((option) => option.value === mode)?.note}
          aside={mode !== '' && <OverrideChip onReset={() => changeMode('')} />}
          control={
            <ChoicePills<InstancePerformanceMode>
              value={mode}
              options={modeOptions}
              ariaLabel="Instance performance mode"
              onChange={changeMode}
            />
          }
        />

        <SettingRow
          title="Runtime"
          description={selectedPreset?.disabled_reason ?? selectedPreset?.detail}
          aside={
            runtimeOverridden && (
              <OverrideChip
                onReset={() => {
                  setJavaPath('');
                  commit({ jvm_preset: '', java_path: '' }, { label: 'runtime', onSuccess: bumpHealth });
                }}
              />
            )
          }
        >
          <div class="cp-runtime-grid">
            <label class="cp-ovr-field">
              <span>JVM preset</span>
              <SelectField<string>
                value={savedPreset}
                ariaLabel="JVM preset"
                onChange={(next) => commit({ jvm_preset: next }, { label: 'JVM preset', onSuccess: bumpHealth })}
                disabled={selectablePresets.length === 0}
                placeholder="Loading"
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
                if (next === savedJavaPath.trim()) return;
                commit(
                  { java_path: next },
                  { label: 'Java runtime', revert: () => setJavaPath(savedJavaPath), onSuccess: bumpHealth },
                );
              }}
            />
            <JvmArgsInput value={jvmArgs} onChange={onArgsChange} />
          </div>
        </SettingRow>

        <SettingRow
          title="Memory"
          description={`${memoryOverridden ? '' : 'Inherits the global default. '}Recommended ${fmtMem(recMin)} to ${fmtMem(recMax)} for this system.`}
          aside={
            memoryOverridden && (
              <OverrideChip
                onReset={() =>
                  commit({ min_memory_mb: 0, max_memory_mb: 0 }, { label: 'memory', onSuccess: bumpHealth })
                }
              />
            )
          }
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
                  label: 'memory',
                  revert: () => {
                    setMinGb(savedMinGb);
                    setMaxGb(savedMaxGb);
                  },
                  onSuccess: bumpHealth,
                },
              )
            }
          />
        </SettingRow>

        <SettingRow
          title="Window"
          description="Game window size when this instance launches."
          aside={
            windowOverridden && (
              <OverrideChip
                onReset={() =>
                  commit({ window_width: 0, window_height: 0 }, { label: 'window size', onSuccess: bumpHealth })
                }
              />
            )
          }
        >
          <WindowField
            width={effectiveWidth}
            height={effectiveHeight}
            inherit={
              windowOverridden
                ? undefined
                : { active: true, label: `Inherits global (${globalWidth} × ${globalHeight})` }
            }
            onCommit={(w, h) =>
              commit({ window_width: w, window_height: h }, { label: 'window size', onSuccess: bumpHealth })
            }
          />
        </SettingRow>
      </SettingsSection>
    </div>
  );
}
