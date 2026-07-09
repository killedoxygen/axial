import type { ComponentChildren, JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Icon } from '../../../ui/Icons';
import { Button } from '../../../ui/Atoms';
import { Segmented } from '../../../ui/Segmented';
import { SelectField } from '../../../ui/Select';
import { type SliderZone } from '../../../ui/Slider';
import { RangeSlider } from '../../../ui/RangeSlider';
import { api } from '../../../api';
import { config, systemInfo } from '../../../store';
import { updateInstanceInList } from '../../../actions';
import { toast } from '../../../toast';
import { errMessage, fmtMem, getMemoryRecommendation } from '../../../utils';
import type { InstancePerformanceMode } from '../../../types-performance';
import type { EnrichedInstance } from '../../../types-instance';
import { memoryGb } from '../format';
import {
  fetchPerformanceHealth,
  globalPerformanceMode,
  performanceModeFrom,
  performanceModeLabel,
} from '../performance-mode';
import type { PerformanceMode } from '../../../types-performance';
import { JavaPathField, JvmArgsInput } from './AdvancedOverrides';
import { WindowSizeField, type WindowPreset } from './WindowSizeField';

const WINDOW_PRESETS: WindowPreset[] = [
  { id: 'default', label: 'Default', w: 854, h: 480 },
  { id: 'hd', label: '720p', w: 1280, h: 720 },
  { id: 'fhd', label: '1080p', w: 1920, h: 1080 },
  { id: '2k', label: '2K', w: 2560, h: 1440 },
];

function windowDimension(value: number | undefined, fallback: number): number {
  return Number.isFinite(value) && (value ?? 0) > 0 ? value! : fallback;
}

const INSTANCE_PERFORMANCE_OPTIONS: Array<{ value: InstancePerformanceMode; label: string; icon: string }> = [
  { value: '', label: 'Inherit', icon: 'globe' },
  { value: 'managed', label: 'Managed', icon: 'sparkles' },
  { value: 'vanilla', label: 'Vanilla', icon: 'cube' },
  { value: 'custom', label: 'Custom', icon: 'sliders' },
];

interface JvmPresetOption {
  id: string;
  label: string;
  detail: string;
  default: boolean;
  disabled_reason?: string | null;
}

interface CreateBackendViewResponse {
  preset_options?: JvmPresetOption[];
}

function instancePerformanceNote(mode: InstancePerformanceMode, globalMode: PerformanceMode): string {
  if (!mode) return `Follows the global Performance setting, currently ${performanceModeLabel(globalMode)}.`;
  if (mode === 'managed') return 'Croopor applies recommended tuning and optimizations for this instance.';
  if (mode === 'vanilla') return 'Pure Minecraft. No tweaks or add-ons applied at launch.';
  return 'You set the tuning. Your manual choices are kept as-is.';
}

function instancePerformanceModeFrom(value: string | undefined): InstancePerformanceMode {
  return performanceModeFrom(value) ?? '';
}

function jvmPresetFromBackendOptions(value: string | undefined, options: JvmPresetOption[]): string {
  const trimmed = (value ?? '').trim();
  const selectable = selectableJvmPresetOptions(options);
  if (selectable.length === 0) return trimmed;
  return selectable.some((option) => option.id === trimmed) ? trimmed : '';
}

function selectableJvmPresetOptions(options: JvmPresetOption[]): JvmPresetOption[] {
  return options.filter((option) => !option.disabled_reason);
}

function jvmPresetSelectLabel(option: JvmPresetOption): string {
  return option.disabled_reason ? `${option.label} (${option.disabled_reason})` : option.label;
}

function SettingRow({
  title,
  description,
  className,
  children,
}: {
  title: string;
  description?: ComponentChildren;
  className?: string;
  children: ComponentChildren;
}): JSX.Element {
  return (
    <div class={`cp-iset-row${className ? ` ${className}` : ''}`}>
      <div class="cp-iset-row-copy">
        <strong>{title}</strong>
        {description && <p>{description}</p>}
      </div>
      <div class="cp-iset-row-control">{children}</div>
    </div>
  );
}

export function SettingsPane({ inst }: { inst: EnrichedInstance }): JSX.Element {
  const [maxMem, setMaxMem] = useState<number>(memoryGb(inst.max_memory_mb, config.value?.max_memory_mb ?? 4096));
  const [minMem, setMinMem] = useState<number>(memoryGb(inst.min_memory_mb, config.value?.min_memory_mb ?? 1024));
  const [width, setWidth] = useState<number>(windowDimension(inst.window_width, 854));
  const [height, setHeight] = useState<number>(windowDimension(inst.window_height, 480));
  const [performanceMode, setPerformanceMode] = useState<InstancePerformanceMode>(
    instancePerformanceModeFrom(inst.performance_mode),
  );
  const [jvmPresetOptions, setJvmPresetOptions] = useState<JvmPresetOption[]>([]);
  const [jvmPreset, setJvmPreset] = useState<string>(jvmPresetFromBackendOptions(inst.jvm_preset, jvmPresetOptions));
  const [javaPath, setJavaPath] = useState<string>(inst.java_path ?? '');
  const [jvmArgs, setJvmArgs] = useState<string>(inst.extra_jvm_args ?? '');
  const [saving, setSaving] = useState(false);
  const [healthRefreshKey, setHealthRefreshKey] = useState(0);
  const [healthNotice, setHealthNotice] = useState<{
    tone: 'warned' | 'error';
    title: string;
    detail: string;
  } | null>(null);
  const globalMode = globalPerformanceMode();
  const totalGB = systemInfo.value?.total_memory_mb
    ? Math.max(1, Math.floor(systemInfo.value.total_memory_mb / 1024))
    : 32;
  const ramMax = Math.max(2, Math.min(32, totalGB));
  const rec = getMemoryRecommendation(totalGB);
  const recMin = Math.min(ramMax, Math.max(1, rec.rec - 2));
  const recMax = Math.min(ramMax, rec.rec + 2);
  const memoryZones: SliderZone[] = [
    { from: 1, to: recMin, tone: 'low', label: 'Low' },
    { from: recMin, to: recMax, tone: 'sweet', label: 'Recommended' },
    { from: recMax, to: Math.min(ramMax, Math.max(recMax, ramMax * 0.75)), tone: 'high', label: 'High' },
    { from: Math.min(ramMax, Math.max(recMax, ramMax * 0.75)), to: ramMax, tone: 'extreme', label: 'Aggressive' },
  ];
  const activeWindowPreset = WINDOW_PRESETS.find((p) => p.w === width && p.h === height)?.id ?? 'custom';
  const activeWindowLabel = WINDOW_PRESETS.find((p) => p.id === activeWindowPreset)?.label ?? 'Custom';
  const effectiveSettingsMode = performanceMode || globalMode;
  const persistedJvmPreset = jvmPresetFromBackendOptions(inst.jvm_preset, jvmPresetOptions);
  const selectableJvmPresets = selectableJvmPresetOptions(jvmPresetOptions);
  const selectedJvmPresetOption =
    jvmPresetOptions.find((option) => option.id === jvmPreset) ??
    jvmPresetOptions.find((option) => option.default) ??
    null;
  const performanceModeText = performanceMode
    ? `${performanceModeLabel(effectiveSettingsMode)} override`
    : `Inherits ${performanceModeLabel(effectiveSettingsMode)} from global settings`;
  const persistedWidth = inst.window_width ?? 854;
  const persistedHeight = inst.window_height ?? 480;
  const dirty =
    Math.round(maxMem * 1024) !== (inst.max_memory_mb ?? config.value?.max_memory_mb ?? 4096) ||
    Math.round(Math.min(minMem, maxMem) * 1024) !== (inst.min_memory_mb ?? config.value?.min_memory_mb ?? 1024) ||
    width !== persistedWidth ||
    height !== persistedHeight ||
    performanceMode !== instancePerformanceModeFrom(inst.performance_mode) ||
    jvmPreset !== persistedJvmPreset ||
    javaPath !== (inst.java_path ?? '') ||
    jvmArgs !== (inst.extra_jvm_args ?? '');

  useEffect(() => {
    setMinMem((prev) => Math.min(prev, maxMem));
  }, [maxMem]);

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

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const res = (await api('GET', '/instances/create-view')) as CreateBackendViewResponse & { error?: string };
        if (cancelled || res.error) return;
        const options = Array.isArray(res.preset_options)
          ? res.preset_options.filter(
              (option): option is JvmPresetOption =>
                typeof option.id === 'string' && typeof option.label === 'string' && typeof option.detail === 'string',
            )
          : [];
        setJvmPresetOptions(options);
        setJvmPreset((current) => jvmPresetFromBackendOptions(current, options));
      } catch {
        if (!cancelled) setJvmPresetOptions([]);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    setMaxMem(memoryGb(inst.max_memory_mb, config.value?.max_memory_mb ?? 4096));
    setMinMem(memoryGb(inst.min_memory_mb, config.value?.min_memory_mb ?? 1024));
    setWidth(windowDimension(inst.window_width, 854));
    setHeight(windowDimension(inst.window_height, 480));
    setPerformanceMode(instancePerformanceModeFrom(inst.performance_mode));
    setJvmPreset(jvmPresetFromBackendOptions(inst.jvm_preset, jvmPresetOptions));
    setJavaPath(inst.java_path ?? '');
    setJvmArgs(inst.extra_jvm_args ?? '');
  }, [
    inst.id,
    inst.max_memory_mb,
    inst.min_memory_mb,
    inst.window_width,
    inst.window_height,
    inst.performance_mode,
    inst.jvm_preset,
    inst.java_path,
    inst.extra_jvm_args,
    jvmPresetOptions,
  ]);

  const save = async (): Promise<void> => {
    setSaving(true);
    try {
      const clampedMinMem = Math.min(minMem, maxMem);
      const res: any = await api('PUT', `/instances/${encodeURIComponent(inst.id)}`, {
        max_memory_mb: Math.round(maxMem * 1024),
        min_memory_mb: Math.round(clampedMinMem * 1024),
        art_seed: inst.art_seed,
        window_width: width,
        window_height: height,
        performance_mode: performanceMode,
        jvm_preset: jvmPreset,
        java_path: javaPath,
        extra_jvm_args: jvmArgs,
      });
      if (res?.error) throw new Error(res.error);
      updateInstanceInList(res);
      setHealthRefreshKey((current) => current + 1);
      toast('Saved instance settings');
    } catch (err) {
      toast(`Could not save instance settings: ${errMessage(err)}`, 'error');
    } finally {
      setSaving(false);
    }
  };

  return (
    <div class="cp-instance-body cp-settings-pane">
      <div class="cp-resource-toolbar cp-settings-toolbar">
        <strong>Instance settings</strong>
        <div class="cp-settings-save">
          <span data-dirty={dirty}>{dirty ? 'Unsaved changes' : 'Up to date'}</span>
          <Button onClick={save} disabled={saving || !dirty} sound="affirm">
            {saving ? 'Saving…' : 'Save settings'}
          </Button>
        </div>
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

      <div class="cp-iset">
        <div class="cp-iset-rows">
          <SettingRow
            title="Launch profile"
            description={
              <>
                {performanceModeText}. {instancePerformanceNote(performanceMode, globalMode)}
              </>
            }
            className="cp-iset-row--performance"
          >
            <div class="cp-iset-seg" aria-label="Instance performance mode">
              <Segmented<InstancePerformanceMode>
                options={INSTANCE_PERFORMANCE_OPTIONS}
                value={performanceMode}
                onChange={setPerformanceMode}
              />
            </div>
          </SettingRow>

          <SettingRow
            title="Runtime"
            description={selectedJvmPresetOption?.disabled_reason ?? selectedJvmPresetOption?.detail}
            className="cp-iset-row--runtime"
          >
            <div class="cp-runtime-control">
              <div class="cp-iset-duo">
                <label class="cp-ovr-field">
                  <span>JVM preset</span>
                  <SelectField<string>
                    value={jvmPreset}
                    ariaLabel="JVM preset"
                    onChange={setJvmPreset}
                    disabled={selectableJvmPresets.length === 0}
                    placeholder="Loading"
                    options={jvmPresetOptions.map((preset) => ({
                      value: preset.id,
                      label: jvmPresetSelectLabel(preset),
                      disabled: Boolean(preset.disabled_reason),
                    }))}
                  />
                </label>

                <JavaPathField value={javaPath} onChange={setJavaPath} />
              </div>

              <JvmArgsInput value={jvmArgs} onChange={setJvmArgs} />
            </div>
          </SettingRow>

          <SettingRow
            title="Memory"
            description={
              <>
                Recommended {fmtMem(recMin)} to {fmtMem(recMax)} for this system.
              </>
            }
            className="cp-iset-row--memory"
          >
            <div class="cp-settings-heap">
              <div class="cp-settings-heap-readout">
                <span>
                  Min <strong>{fmtMem(minMem)}</strong>
                </span>
                <span class="cp-settings-heap-band">{fmtMem(maxMem - minMem)} elastic</span>
                <span>
                  Max <strong>{fmtMem(maxMem)}</strong>
                </span>
              </div>
              <div class="cp-settings-range-wrap">
                <RangeSlider
                  low={minMem}
                  high={maxMem}
                  min={1}
                  max={ramMax}
                  step={0.5}
                  zones={memoryZones}
                  sound="memory"
                  onChange={(low, high) => {
                    setMinMem(low);
                    setMaxMem(high);
                  }}
                  ariaLabelLow="Minimum heap in gigabytes"
                  ariaLabelHigh="Maximum heap in gigabytes"
                />
              </div>
            </div>
          </SettingRow>

          <SettingRow
            title="Window"
            description={`${activeWindowLabel} · ${width} x ${height}.`}
            className="cp-iset-row--window"
          >
            <WindowSizeField
              width={width}
              height={height}
              presets={WINDOW_PRESETS}
              onChange={(w, h) => {
                setWidth(w);
                setHeight(h);
              }}
            />
          </SettingRow>
        </div>
      </div>
    </div>
  );
}
