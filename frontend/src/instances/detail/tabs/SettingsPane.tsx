import type { JSX } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { Chip } from '../../../ui/Atoms';
import { SelectField } from '../../../ui/Select';
import { ChoicePills, type ChoicePillOption } from '../../../ui/ChoicePills';
import { OverrideChip, SettingRow, SettingsSection } from '../../../ui/SettingsSheet';
import { MemoryField, recommendedHeapRange } from '../../../ui/MemoryField';
import { WindowField } from '../../../ui/WindowField';
import { JavaPathField, JvmArgsInput } from '../../../launch/RuntimeFields';
import { canAutoSave, useAutoSave } from '../../../settings/autosave';
import { normalizeJvmPreset, useJvmPresets } from '../../../launch/jvm-presets';
import { updateInstance } from '../../api';
import { explicitMaxMemoryMb, isAutomaticMemory } from '../../../settings/memory';
import { refreshInstanceReadiness } from '../../readiness';
import { config, systemInfo } from '../../../settings/state';
import { instances, updateInstanceInList } from '../../state';
import { fmtMem, memoryGb } from '../../../ui/format';
import type { ConfigPerformanceMode } from '../../../generated/ConfigPerformanceMode';
import type { EnrichedInstance } from '../../../generated/EnrichedInstance';
import type { JavaSelection } from '../../../generated/JavaSelection';
import {
  globalPerformanceMode,
  performanceModeFrom,
  performanceModeLabel,
  performanceModeNotes,
} from '../performance-mode';

type InstancePerformanceMode = ConfigPerformanceMode | '';
const INHERITED_JAVA: JavaSelection = { kind: 'inherited' };

function instancePerformanceModeFrom(value: string | undefined): InstancePerformanceMode {
  return performanceModeFrom(value) ?? '';
}

function windowDimension(value: number | undefined, fallback: number): number {
  return Number.isFinite(value) && (value ?? 0) > 0 ? value! : fallback;
}

export function SettingsPane({ inst }: { inst: EnrichedInstance }): JSX.Element {
  return <InstanceSettingsPane key={inst.id} inst={inst} />;
}

function InstanceSettingsPane({ inst }: { inst: EnrichedInstance }): JSX.Element {
  const cfg = config.value;
  const globalMode = globalPerformanceMode();
  const totalGb = systemInfo.value?.total_memory_mb
    ? Math.max(1, Math.floor(systemInfo.value.total_memory_mb / 1024))
    : 32;
  const [recMin, recMax] = recommendedHeapRange(totalGb);
  const flushArgs = useRef<() => void>(() => {});

  const { commit } = useAutoSave<EnrichedInstance>({
    send: (patch) => updateInstance(inst.id, patch),
    apply: (res) => updateInstanceInList(res),
    errorLabel: 'instance settings',
    target: `/instances/${inst.id}`,
    instance: { id: inst.id, revision: inst.revision },
    flushPending: () => flushArgs.current(),
  });

  const memoryOverridden = (inst.max_memory_mb ?? 0) > 0 || (inst.min_memory_mb ?? 0) > 0;
  const savedMaxGb = memoryGb(inst.max_memory_mb, explicitMaxMemoryMb(cfg));
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

  const presetOptions = useJvmPresets();
  const savedPreset = normalizeJvmPreset(inst.jvm_preset, presetOptions);
  const selectedPreset =
    presetOptions.find((option) => option.id === savedPreset) ?? presetOptions.find((option) => option.default) ?? null;

  const javaSelection = inst.java_selection ?? INHERITED_JAVA;
  const savedJavaComponent = javaSelection.kind === 'component' ? javaSelection.component : undefined;
  const [javaPath, setJavaPath] = useState('');
  const runtimeOverridden = savedPreset !== '' || javaSelection.kind !== 'inherited';

  const savedArgs = inst.extra_jvm_args ?? '';
  const [jvmArgs, setJvmArgs] = useState(savedArgs);
  const argsTimer = useRef<number | null>(null);
  const pendingArgs = useRef<string | null>(null);

  useEffect(() => {
    setMaxGb(savedMaxGb);
    setMinGb(savedMinGb);
  }, [savedMaxGb, savedMinGb]);

  useEffect(() => {
    setMode(savedMode);
  }, [savedMode]);

  useEffect(() => {
    setJavaPath('');
  }, [javaSelection.kind, savedJavaComponent]);

  useEffect(() => {
    if (pendingArgs.current === null) setJvmArgs(savedArgs);
  }, [savedArgs]);

  useEffect(() => {
    let cancelled = false;
    void refreshInstanceReadiness(inst.id, { isCurrent: () => !cancelled, retry: false }).catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [inst.id]);

  const commitArgs = (next: string): void => {
    if (!canAutoSave()) return;
    pendingArgs.current = null;
    if (next === savedArgs) return;
    commit({ extra_jvm_args: next }, { label: 'JVM arguments', revert: () => setJvmArgs(savedArgs) });
  };

  const onArgsChange = (next: string): void => {
    if (!canAutoSave()) return;
    setJvmArgs(next);
    pendingArgs.current = next;
    if (argsTimer.current !== null) window.clearTimeout(argsTimer.current);
    argsTimer.current = window.setTimeout(() => {
      argsTimer.current = null;
      flushArgs.current();
    }, 600);
  };

  flushArgs.current = () => {
    if (argsTimer.current !== null) {
      window.clearTimeout(argsTimer.current);
      argsTimer.current = null;
    }
    if (pendingArgs.current !== null) commitArgs(pendingArgs.current);
  };

  const modeOptions: Array<ChoicePillOption<InstancePerformanceMode>> = [
    {
      value: '',
      label: 'Inherit',
      note: `Follows the global Performance setting, currently ${performanceModeLabel(globalMode)}.`,
    },
    { value: 'managed', label: 'Managed', note: performanceModeNotes.managed },
    { value: 'vanilla', label: 'Vanilla', note: performanceModeNotes.vanilla },
    { value: 'custom', label: 'Custom', note: performanceModeNotes.custom },
  ];

  const changeMode = (next: InstancePerformanceMode): void => {
    setMode(next);
    commit(
      { performance_mode: next },
      {
        label: 'launch profile',
        revert: () =>
          setMode(
            instancePerformanceModeFrom(instances.value.find((instance) => instance.id === inst.id)?.performance_mode),
          ),
      },
    );
  };

  return (
    <div class="cp-pane cp-settings-pane">
      <SettingsSection>
        <SettingRow
          title="Memory"
          description={`Recommended ${fmtMem(recMin)} to ${fmtMem(recMax)} for this system.`}
          aside={
            memoryOverridden ? (
              <OverrideChip onReset={() => commit({ min_memory_mb: 0, max_memory_mb: 0 }, { label: 'memory' })} />
            ) : (
              <Chip>{isAutomaticMemory(cfg) ? 'Automatic' : 'Global'}</Chip>
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
                    const current = instances.value.find((instance) => instance.id === inst.id);
                    setMinGb(memoryGb(current?.min_memory_mb, config.value?.min_memory_mb ?? 1024));
                    setMaxGb(memoryGb(current?.max_memory_mb, explicitMaxMemoryMb(config.value)));
                  },
                },
              )
            }
          />
        </SettingRow>

        <SettingRow
          title="Game window"
          description="Game window size when this instance launches."
          aside={
            windowOverridden && (
              <OverrideChip onReset={() => commit({ window_width: 0, window_height: 0 }, { label: 'window size' })} />
            )
          }
        >
          <WindowField
            width={effectiveWidth}
            height={effectiveHeight}
            inherit={
              windowOverridden ? undefined : { active: true, label: `Global size\n${globalWidth} × ${globalHeight}` }
            }
            onCommit={(w, h) => commit({ window_width: w, window_height: h }, { label: 'window size' })}
          />
        </SettingRow>

        <SettingRow
          title="Runtime"
          description={selectedPreset?.detail}
          aside={
            runtimeOverridden && (
              <OverrideChip
                onReset={() => {
                  setJavaPath('');
                  commit({ jvm_preset: '', java_path: '' }, { label: 'runtime', revert: () => setJavaPath('') });
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
                onChange={(next) => commit({ jvm_preset: next }, { label: 'JVM preset' })}
                disabled={presetOptions.length === 0}
                placeholder="Loading"
                options={presetOptions.map((preset) => ({
                  value: preset.id,
                  label: preset.label,
                }))}
              />
            </label>
            <JavaPathField
              value={javaPath}
              selection={javaSelection}
              onChange={setJavaPath}
              onCommit={(next) => {
                if ((next === '' && javaSelection.kind === 'inherited') || next === savedJavaComponent) return;
                commit({ java_path: next }, { label: 'Java runtime', revert: () => setJavaPath('') });
              }}
            />
            <JvmArgsInput value={jvmArgs} onChange={onArgsChange} />
          </div>
        </SettingRow>

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
      </SettingsSection>
    </div>
  );
}
