import type { JSX } from 'preact';
import type { EnrichedInstance, Config } from '../../types';
import { api } from '../../api';
import { Sound } from '../../sound';

function jvmPresetLabel(preset: string): string | null {
  if (preset === '') return 'Auto JVM';
  if (preset === 'smooth') return 'Smooth GC';
  if (preset === 'performance') return 'Performance GC';
  if (preset === 'ultra_low_latency') return 'Ultra Low Latency';
  if (preset === 'graalvm') return 'GraalVM';
  if (preset === 'legacy') return 'Legacy GC';
  if (preset === 'legacy_pvp') return 'Legacy PvP GC';
  if (preset === 'legacy_heavy') return 'Legacy Heavy GC';
  if (preset === 'aikar') return "Aikar's Flags";
  if (preset === 'zgc') return 'ZGC';
  return null;
}

export function AdvancedPane({ inst, cfg, isVanilla, javaPath, jvmPreset, extraJvmArgs, saving, onJavaPath, onJvmPreset, onExtraJvmArgs, onSave, onReset }: {
  inst: EnrichedInstance;
  cfg: Config | null;
  isVanilla: boolean;
  javaPath: string;
  jvmPreset: string;
  extraJvmArgs: string;
  saving: boolean;
  onJavaPath: (v: string) => void;
  onJvmPreset: (v: string) => void;
  onExtraJvmArgs: (v: string) => void;
  onSave: () => void;
  onReset: () => void;
}): JSX.Element {
  const inheritedPreset = jvmPresetLabel(cfg?.jvm_preset || '') || 'Auto JVM';
  const hasChanges = javaPath !== (inst.java_path || '')
    || jvmPreset !== (inst.jvm_preset || '')
    || extraJvmArgs !== (inst.extra_jvm_args || '');

  const handleLinkClick = (sub: string) => {
    api('POST', `/instances/${encodeURIComponent(inst.id)}/open-folder${sub ? '?sub=' + sub : ''}`);
    Sound.ui('click');
  };

  return (
    <div class="advanced-pane">
      <div class="advanced-section">
        <div class="advanced-section-title">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M4 10a2 2 0 1 0 4 0a2 2 0 0 0-4 0" /><path d="M6 4v4" /><path d="M6 12v8" /><path d="M10 16a2 2 0 1 0 4 0a2 2 0 0 0-4 0" /><path d="M12 4v10" /><path d="M12 18v2" /><path d="M16 7a2 2 0 1 0 4 0a2 2 0 0 0-4 0" /><path d="M18 4v1" /><path d="M18 9v11" /></svg>
          Launch Overrides
        </div>
        <div class="setting-group">
          <label class="setting-label" for="instance-java-path">Java Path</label>
          <input
            id="instance-java-path"
            class="setting-input"
            type="text"
            autocomplete="off"
            placeholder={cfg?.java_path_override || 'Inherit global Java'}
            value={javaPath}
            onInput={(e) => onJavaPath((e.currentTarget as HTMLInputElement).value)}
          />
          <div class="setting-hint">
            Global: {cfg?.java_path_override || 'auto-detected managed runtime'}
          </div>
        </div>
        <div class="setting-group">
          <label class="setting-label" for="instance-jvm-preset">JVM Preset</label>
          <select
            id="instance-jvm-preset"
            class="setting-input"
            value={jvmPreset}
            onChange={(e) => onJvmPreset((e.currentTarget as HTMLSelectElement).value)}
          >
            <option value="">Inherit global ({inheritedPreset})</option>
            <option value="smooth">Smooth</option>
            <option value="performance">Performance</option>
            <option value="ultra_low_latency">Ultra Low Latency</option>
            <option value="graalvm">GraalVM</option>
            <option value="legacy">Legacy</option>
            <option value="legacy_pvp">Legacy PvP</option>
            <option value="legacy_heavy">Legacy Heavy</option>
          </select>
          <div class="setting-hint">
            Per-instance presets are treated as explicit manual choices by the self-healing layer.
          </div>
        </div>
        <div class="setting-group">
          <label class="setting-label" for="instance-extra-jvm-args">Extra JVM Args</label>
          <textarea
            id="instance-extra-jvm-args"
            class="setting-input advanced-textarea"
            autocomplete="off"
            placeholder="-XX:+UnlockExperimentalVMOptions -XX:G1NewSizePercent=30"
            value={extraJvmArgs}
            onInput={(e) => onExtraJvmArgs((e.currentTarget as HTMLTextAreaElement).value)}
          />
          <div class="setting-hint">
            Appended to the computed flags. Use for testing self-healing guardrails.
          </div>
        </div>
        <div class="advanced-actions">
          <button
            type="button"
            class="btn-secondary"
            disabled={saving || (!javaPath && !jvmPreset && !extraJvmArgs)}
            onClick={onReset}
          >
            Clear
          </button>
          <button
            type="button"
            class="btn-primary"
            disabled={saving || !hasChanges}
            onClick={onSave}
          >
            {saving ? 'Saving...' : 'Save Overrides'}
          </button>
        </div>
      </div>

      <div class="advanced-section">
        <div class="advanced-section-title">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" /></svg>
          Quick Access
        </div>
        <div class="advanced-links">
          <button type="button" class="advanced-link-btn" onClick={() => handleLinkClick('saves')}>
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12a9 9 0 1 0 18 0a9 9 0 0 0-18 0" /><path d="M3.6 9h16.8" /><path d="M3.6 15h16.8" /><path d="M11.5 3a17 17 0 0 0 0 18" /><path d="M12.5 3a17 17 0 0 1 0 18" /></svg>
            Saves
          </button>
          <button
            type="button"
            class={`advanced-link-btn${isVanilla ? ' disabled' : ''}`}
            disabled={isVanilla}
            {...(!isVanilla ? { onClick: () => handleLinkClick('mods') } : {})}
            {...(isVanilla ? { title: 'No mod loader installed' } : {})}
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h3a1 1 0 0 0 1-1v-1a2 2 0 0 1 4 0v1a1 1 0 0 0 1 1h3a1 1 0 0 1 1 1v3a1 1 0 0 0 1 1h1a2 2 0 0 1 0 4h-1a1 1 0 0 0-1 1v3a1 1 0 0 1-1 1h-3a1 1 0 0 1-1-1v-1a2 2 0 0 0-4 0v1a1 1 0 0 1-1 1h-3a1 1 0 0 1-1-1v-3a1 1 0 0 1 1-1h1a2 2 0 0 0 0-4h-1a1 1 0 0 1-1-1v-3a1 1 0 0 1 1-1" /></svg>
            Mods
          </button>
          <button type="button" class="advanced-link-btn" onClick={() => handleLinkClick('resourcepacks')}>
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 21a9 9 0 0 1 0-18c4.97 0 9 3.582 9 8c0 1.06-.474 2.078-1.318 2.828a4.007 4.007 0 0 1-2.682 1.172h-2.5a2 2 0 0 0-1 3.75a1.3 1.3 0 0 1-1.5 1.25" /><path d="M8.5 10.5m-1 0a1 1 0 1 0 2 0a1 1 0 1 0-2 0" /><path d="M12.5 7.5m-1 0a1 1 0 1 0 2 0a1 1 0 1 0-2 0" /><path d="M16.5 10.5m-1 0a1 1 0 1 0 2 0a1 1 0 1 0-2 0" /></svg>
            Resources
          </button>
          <button type="button" class="advanced-link-btn" onClick={() => handleLinkClick('config')}>
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3" /><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" /></svg>
            Config
          </button>
          <button type="button" class="advanced-link-btn" onClick={() => handleLinkClick('')}>
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" /></svg>
            Instance Root
          </button>
        </div>
      </div>
    </div>
  );
}
