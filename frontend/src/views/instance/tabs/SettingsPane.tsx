import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Button, Card, Input, Segmented } from '../../../ui/Atoms';
import { Slider, type SliderZone } from '../../../ui/Slider';
import { InstanceArt, artPresetForSeed, artSeedFor, nextArtSeed } from '../../../art/InstanceArt';
import { api } from '../../../api';
import { config, systemInfo } from '../../../store';
import { updateInstanceInList } from '../../../actions';
import { toast } from '../../../toast';
import { errMessage, fmtMem, getMemoryRecommendation } from '../../../utils';
import type { EnrichedInstance, InstancePerformanceMode } from '../../../types';
import {
  JVM_PRESET_HINTS,
  JVM_PRESET_LABELS,
  JVM_PRESET_ORDER,
  jvmPresetFrom,
  type JvmPreset,
} from '../../create/jvm-presets';
import { memoryGb } from '../format';
import { globalPerformanceMode, performanceModeFrom, performanceModeLabel } from '../performance-mode';

type InstanceWindowPreset = { id: string; label: string; w: number; h: number };

const WINDOW_PRESETS: InstanceWindowPreset[] = [
  { id: 'default', label: 'Default', w: 854, h: 480 },
  { id: 'hd', label: '720p', w: 1280, h: 720 },
  { id: 'fhd', label: '1080p', w: 1920, h: 1080 },
  { id: '2k', label: '2K', w: 2560, h: 1440 },
];

const INSTANCE_PERFORMANCE_OPTIONS: Array<{ value: InstancePerformanceMode; label: string }> = [
  { value: '', label: 'Inherit' },
  { value: 'managed', label: 'Managed' },
  { value: 'vanilla', label: 'Vanilla' },
  { value: 'custom', label: 'Custom' },
];

function clampWindowDimension(value: string, fallback: number): number {
  const parsed = Number.parseInt(value, 10);
  if (!Number.isFinite(parsed)) return fallback;
  return Math.max(320, Math.min(3840, parsed));
}

function instancePerformanceModeFrom(value: string | undefined): InstancePerformanceMode {
  return performanceModeFrom(value) ?? '';
}

export function SettingsPane({ inst }: { inst: EnrichedInstance }): JSX.Element {
  const initialArtSeed = artSeedFor(inst);
  const [artSeed, setArtSeed] = useState<number>(initialArtSeed);
  const artPreset = artPresetForSeed(artSeed);
  const [maxMem, setMaxMem] = useState<number>(memoryGb(inst.max_memory_mb, config.value?.max_memory_mb ?? 4096));
  const [minMem, setMinMem] = useState<number>(memoryGb(inst.min_memory_mb, config.value?.min_memory_mb ?? 1024));
  const [width, setWidth] = useState<number>(inst.window_width ?? 854);
  const [height, setHeight] = useState<number>(inst.window_height ?? 480);
  const [performanceMode, setPerformanceMode] = useState<InstancePerformanceMode>(instancePerformanceModeFrom(inst.performance_mode));
  const [jvmPreset, setJvmPreset] = useState<JvmPreset>(jvmPresetFrom(inst.jvm_preset));
  const [javaPath, setJavaPath] = useState<string>(inst.java_path ?? '');
  const [jvmArgs, setJvmArgs] = useState<string>(inst.extra_jvm_args ?? '');
  const [saving, setSaving] = useState(false);
  const totalGB = systemInfo.value?.total_memory_mb ? Math.max(1, Math.floor(systemInfo.value.total_memory_mb / 1024)) : 32;
  const ramMax = Math.max(2, Math.min(32, totalGB));
  const rec = getMemoryRecommendation(totalGB);
  const recMin = Math.min(ramMax, Math.max(1, rec.rec - 2));
  const recMax = Math.min(ramMax, rec.rec + 2);
  const memoryZones: SliderZone[] = [
    { from: 0.5, to: recMin, tone: 'low', label: 'Low' },
    { from: recMin, to: recMax, tone: 'sweet', label: 'Recommended' },
    { from: recMax, to: Math.min(ramMax, Math.max(recMax, ramMax * 0.75)), tone: 'high', label: 'High' },
    { from: Math.min(ramMax, Math.max(recMax, ramMax * 0.75)), to: ramMax, tone: 'extreme', label: 'Aggressive' },
  ];
  const activeWindowPreset = WINDOW_PRESETS.find(p => p.w === width && p.h === height)?.id ?? 'custom';
  const activeWindowLabel = WINDOW_PRESETS.find(p => p.id === activeWindowPreset)?.label ?? 'Custom';
  const effectiveSettingsMode = performanceMode || globalPerformanceMode();
  const performanceModeText = performanceMode
    ? `${performanceModeLabel(effectiveSettingsMode)} override`
    : `Inherits ${performanceModeLabel(effectiveSettingsMode)} from global settings`;
  const runtimePresetText = `${JVM_PRESET_LABELS[jvmPreset]}: ${JVM_PRESET_HINTS[jvmPreset]}`;
  const dirty = (
    artSeed !== initialArtSeed ||
    Math.round(maxMem * 1024) !== (inst.max_memory_mb ?? config.value?.max_memory_mb ?? 4096) ||
    Math.round(Math.min(minMem, maxMem) * 1024) !== (inst.min_memory_mb ?? config.value?.min_memory_mb ?? 1024) ||
    width !== (inst.window_width ?? 854) ||
    height !== (inst.window_height ?? 480) ||
    performanceMode !== instancePerformanceModeFrom(inst.performance_mode) ||
    jvmPreset !== jvmPresetFrom(inst.jvm_preset) ||
    javaPath !== (inst.java_path ?? '') ||
    jvmArgs !== (inst.extra_jvm_args ?? '')
  );

  useEffect(() => {
    setMinMem(prev => Math.min(prev, maxMem));
  }, [maxMem]);

  useEffect(() => {
    const nextSeed = artSeedFor(inst);
    setArtSeed(nextSeed);
    setMaxMem(memoryGb(inst.max_memory_mb, config.value?.max_memory_mb ?? 4096));
    setMinMem(memoryGb(inst.min_memory_mb, config.value?.min_memory_mb ?? 1024));
    setWidth(inst.window_width ?? 854);
    setHeight(inst.window_height ?? 480);
    setPerformanceMode(instancePerformanceModeFrom(inst.performance_mode));
    setJvmPreset(jvmPresetFrom(inst.jvm_preset));
    setJavaPath(inst.java_path ?? '');
    setJvmArgs(inst.extra_jvm_args ?? '');
  }, [
    inst.id,
    inst.art_seed,
    inst.max_memory_mb,
    inst.min_memory_mb,
    inst.window_width,
    inst.window_height,
    inst.performance_mode,
    inst.jvm_preset,
    inst.java_path,
    inst.extra_jvm_args,
  ]);

  const save = async (): Promise<void> => {
    setSaving(true);
    try {
      const clampedMinMem = Math.min(minMem, maxMem);
      const res: any = await api('PUT', `/instances/${encodeURIComponent(inst.id)}`, {
        max_memory_mb: Math.round(maxMem * 1024),
        min_memory_mb: Math.round(clampedMinMem * 1024),
        art_seed: artSeed,
        window_width: width,
        window_height: height,
        performance_mode: performanceMode,
        jvm_preset: jvmPreset,
        java_path: javaPath,
        extra_jvm_args: jvmArgs,
      });
      if (res?.error) throw new Error(res.error);
      updateInstanceInList(res);
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
          <Button onClick={save} disabled={saving || !dirty} sound="affirm">{saving ? 'Saving…' : 'Save settings'}</Button>
        </div>
      </div>

      <div class="cp-iset cp-iset--bento">
        <div id="cp-settings-policy" class="cp-iset-slot cp-iset-slot--policy">
          <Card padding={18} class="cp-iset-card">
            <div class="cp-iset-head">
              <div>
                <h3>Performance policy</h3>
                <p>{performanceModeText}.</p>
              </div>
            </div>
            <div class="cp-iset-body">
              <div class="cp-settings-segment" aria-label="Instance performance mode">
                <Segmented<InstancePerformanceMode>
                  options={INSTANCE_PERFORMANCE_OPTIONS}
                  value={performanceMode}
                  onChange={setPerformanceMode}
                />
              </div>
              <div class="cp-settings-mode-note">
                {performanceMode
                  ? 'This instance will use its own performance mode.'
                  : 'This instance follows the global Performance setting.'}
              </div>
            </div>
          </Card>
        </div>

        <div id="cp-settings-memory" class="cp-iset-slot cp-iset-slot--memory">
          <Card padding={18} class="cp-iset-card">
            <div class="cp-iset-head">
              <div>
                <h3>Memory</h3>
                <p>Recommended range: {fmtMem(recMin)} to {fmtMem(recMax)}.</p>
              </div>
            </div>
            <div class="cp-iset-body">
              <div class="cp-settings-memory-grid">
                <div class="cp-settings-slider-row">
                  <div class="cp-settings-slider-label">
                    <span>Maximum heap</span>
                    <strong>{fmtMem(maxMem)}</strong>
                  </div>
                  <Slider
                    value={maxMem}
                    min={1}
                    max={ramMax}
                    step={0.5}
                    zones={memoryZones}
                    sound="memory"
                    onChange={setMaxMem}
                    ariaLabel="Maximum heap in gigabytes"
                  />
                </div>
                <div class="cp-settings-slider-row">
                  <div class="cp-settings-slider-label">
                    <span>Minimum heap</span>
                    <strong>{fmtMem(minMem)}</strong>
                  </div>
                  <Slider
                    value={minMem}
                    min={0.5}
                    max={maxMem}
                    step={0.5}
                    sound="memory"
                    onChange={setMinMem}
                    ariaLabel="Minimum heap in gigabytes"
                  />
                </div>
              </div>
            </div>
          </Card>
        </div>

        <div id="cp-settings-runtime" class="cp-iset-slot cp-iset-slot--runtime">
          <Card padding={18} class="cp-iset-card">
            <div class="cp-iset-head">
              <div>
                <h3>Runtime</h3>
                <p>{runtimePresetText}</p>
              </div>
            </div>
            <div class="cp-iset-body">
              <div class="cp-settings-runtime-presets" role="radiogroup" aria-label="Runtime preset">
                {JVM_PRESET_ORDER.map((preset) => (
                  <button
                    key={preset || 'auto'}
                    type="button"
                    role="radio"
                    aria-checked={jvmPreset === preset}
                    class="cp-settings-runtime-preset"
                    data-active={jvmPreset === preset}
                    onClick={() => setJvmPreset(preset)}
                    title={`${JVM_PRESET_LABELS[preset]}: ${JVM_PRESET_HINTS[preset]}`}
                  >
                    <span class="cp-settings-runtime-preset-label">{JVM_PRESET_LABELS[preset]}</span>
                    <span class="cp-settings-runtime-preset-hint">{JVM_PRESET_HINTS[preset]}</span>
                  </button>
                ))}
              </div>
              <div class="cp-settings-advanced-label">Advanced overrides</div>
              <div class="cp-settings-advanced-grid">
                <label>
                  <span>Java path</span>
                  <Input value={javaPath} onChange={setJavaPath} placeholder="Managed Java" />
                </label>
                <label>
                  <span>Extra JVM arguments</span>
                  <Input value={jvmArgs} onChange={setJvmArgs} placeholder="-Dfoo=bar -Xss2m" />
                </label>
              </div>
            </div>
          </Card>
        </div>

        <div id="cp-settings-window" class="cp-iset-slot cp-iset-slot--window">
          <Card padding={18} class="cp-iset-card">
            <div class="cp-iset-head">
              <div>
                <h3>Window</h3>
                <p>{activeWindowLabel} · {width} × {height}</p>
              </div>
            </div>
            <div class="cp-iset-body">
              <div class="cp-settings-segment" aria-label="Window size">
                <Segmented<string>
                  options={WINDOW_PRESETS.map((preset) => ({ value: preset.id, label: preset.label }))}
                  value={activeWindowPreset}
                  onChange={(presetId) => {
                    const preset = WINDOW_PRESETS.find((item) => item.id === presetId);
                    if (preset) {
                      setWidth(preset.w);
                      setHeight(preset.h);
                    }
                  }}
                />
              </div>
              <div class="cp-settings-dimensions">
                <label>
                  <span>Width</span>
                  <Input
                    type="number"
                    value={String(width)}
                    onChange={(v) => setWidth(clampWindowDimension(v, width))}
                  />
                </label>
                <label>
                  <span>Height</span>
                  <Input
                    type="number"
                    value={String(height)}
                    onChange={(v) => setHeight(clampWindowDimension(v, height))}
                  />
                </label>
              </div>
            </div>
          </Card>
        </div>

        <div id="cp-settings-identity" class="cp-iset-slot cp-iset-slot--identity">
          <Card padding={18} class="cp-iset-card">
            <div class="cp-iset-head">
              <div>
                <h3>Identity</h3>
                <p>Artwork used for this instance.</p>
              </div>
            </div>
            <div class="cp-iset-body cp-settings-identity-control">
              <InstanceArt
                instance={{ ...inst, art_seed: artSeed }}
                aspect="square"
                radius={12}
                className="cp-settings-avatar"
              />
              <div>
                <strong>{artPreset}</strong>
                <span>Current style</span>
              </div>
              <Button variant="secondary" size="sm" icon="refresh" onClick={() => setArtSeed(seed => nextArtSeed(seed))}>
                Regenerate
              </Button>
            </div>
          </Card>
        </div>
      </div>
    </div>
  );
}
