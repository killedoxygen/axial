import type { JSX } from 'preact';
import { useEffect } from 'preact/hooks';
import { devMode } from '../store';
import { PRESET_HUES, local, localStateVersion, saveLocalState } from '../state';
import { Music, musicStateVersion } from '../music';
import { Sound, playSliderSound } from '../sound';
import { applyTheme, findFixedLightness, initColorField, isLowContrastTheme } from '../theme';
import {
  settingsJavaPath, settingsJavaRuntimes, settingsJavaRuntimesState, settingsJvmPreset,
  settingsWindowHeight, settingsWindowWidth,
} from '../settings';
import { recordingShortcut, resetShortcut, Shortcuts, startRecording } from '../shortcuts';

const THEME_SWATCHS = [
  { theme: 'obsidian', title: 'Obsidian', background: '#0c0e11', border: '#3dd68c', label: 'Obsidian' },
  { theme: 'deepslate', title: 'Deepslate', background: '#101218', border: '#6ea8fe', label: 'Deepslate' },
  { theme: 'nether', title: 'Nether', background: '#140a0a', border: '#ff6b4a', label: 'Nether' },
  { theme: 'end', title: 'The End', background: '#0d0b14', border: '#c4a3ff', label: 'The End' },
  { theme: 'birch', title: 'Birch', background: '#f5f0e8', border: '#5a8f4a', label: 'Birch' },
] as const;

const JVM_PRESETS = [
  { value: '', label: 'Default', hint: null, tip: null },
  {
    value: 'aikar',
    label: "Aikar's Flags",
    hint: '(optimized G1GC)',
    tip: 'Widely used in the MC community for better GC performance.',
  },
  {
    value: 'zgc',
    label: 'ZGC Low-Latency',
    hint: '(Java 17+)',
    tip: 'Eliminates GC pauses but requires Java 17 or newer. You will still be able to launch older versions, but you will not benefit from the setting on them.',
  },
] as const;

function markerStyle(): JSX.CSSProperties {
  return {
    left: `${(local.customHue / 360) * 100}%`,
    top: `${(1 - local.customVibrancy / 100) * 100}%`,
    background: `hsl(${local.customHue},65%,55%)`,
  };
}

export function SettingsView(): JSX.Element {
  localStateVersion.value;
  musicStateVersion.value;
  const isDevMode = devMode.value;
  const javaRuntimeState = settingsJavaRuntimesState.value;
  const javaRuntimes = settingsJavaRuntimes.value;
  const isLowContrast = isLowContrastTheme(
    local.theme === 'custom' ? local.customHue : (PRESET_HUES[local.theme] || 140),
    local.customVibrancy,
    local.lightness,
  );

  useEffect(() => {
    initColorField(
      document.getElementById('color-field') as HTMLElement | null,
      document.getElementById('color-field-marker') as HTMLElement | null,
      (hue: number, vibrancy: number) => {
        applyTheme('custom', hue, { silent: true, vibrancy });
        playSliderSound(hue / 360, 'hue');
      },
      () => applyTheme('custom', local.customHue, { vibrancy: local.customVibrancy }),
    );
  }, []);

  return (
    <div class="settings-main-panel">
      <div class="settings-page-header">
        <div>
          <span class="settings-page-kicker">Croopor</span>
          <h2 class="settings-page-title">Launcher Settings</h2>
        </div>
      </div>

      <div class="settings-content" id="settings-content">
        <section class="settings-section-card" id="settings-section-appearance">
          <div class="settings-section-head">
            <span class="settings-section-kicker">Appearance</span>
            <h3 class="settings-section-title">Theme and feedback</h3>
          </div>

          <div class="setting-group">
            <label class="setting-label">Theme</label>
            <div class="theme-picker" id="theme-picker">
              {THEME_SWATCHS.map((swatch) => (
                <button
                  type="button"
                  class={`theme-swatch${local.theme === swatch.theme ? ' active' : ''}`}
                  data-theme={swatch.theme}
                  title={swatch.title}
                  onClick={() => applyTheme(swatch.theme, null)}
                >
                  <span
                    class="swatch-color"
                    style={{ background: swatch.background, borderColor: swatch.border }}
                  />
                  <span class="swatch-name">{swatch.label}</span>
                </button>
              ))}
            </div>
          </div>

          <div class="setting-group">
            <label class="setting-label">Custom Color</label>
            <div class="color-field" id="color-field">
              <div class="color-field-marker" id="color-field-marker" style={markerStyle()} />
            </div>
            <div class="lightness-row">
              <svg class="lightness-icon" width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z" /></svg>
              <input
                type="range"
                id="lightness-slider"
                class="lightness-slider"
                min="0"
                max="100"
                step="1"
                value={String(local.lightness)}
                onInput={(e) => {
                  const lightness = parseInt((e.currentTarget as HTMLInputElement).value, 10);
                  applyTheme(local.theme, null, { silent: true, lightness });
                  playSliderSound(lightness / 100, 'hue');
                }}
                onChange={(e) => {
                  const lightness = parseInt((e.currentTarget as HTMLInputElement).value, 10);
                  applyTheme(local.theme, null, { lightness });
                }}
              />
              <svg class="lightness-icon" width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="12" cy="12" r="5" /><path d="M12 1v2M12 21v2M4.22 4.22l1.42 1.42M18.36 18.36l1.42 1.42M1 12h2M21 12h2M4.22 19.78l1.42-1.42M18.36 5.64l1.42-1.42" /></svg>
            </div>
            <div class={`wcag-warning${isLowContrast ? '' : ' hidden'}`} id="wcag-warning">
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z" /><line x1="12" y1="9" x2="12" y2="13" /><line x1="12" y1="17" x2="12.01" y2="17" /></svg>
              <span>Low contrast, text may be hard to read.</span>
              <button
                class="wcag-fix-btn"
                id="wcag-fix-btn"
                type="button"
                onClick={() => {
                  const hue = local.theme === 'custom' ? local.customHue : (PRESET_HUES[local.theme] || 140);
                  const fixed = findFixedLightness(hue, local.customVibrancy, local.lightness);
                  applyTheme(local.theme, null, { lightness: fixed });
                }}
              >
                Fix
              </button>
            </div>
          </div>

          <div class="setting-group">
            <label class="setting-label">Sounds</label>
            <label class="toggle-label">
              <input
                type="checkbox"
                id="sounds-toggle"
                checked={local.sounds}
                onChange={(e) => {
                  const next = (e.currentTarget as HTMLInputElement).checked;
                  if (next) {
                    Sound.enabled = true;
                    Sound.ui('theme');
                  } else {
                    Sound.ui('soft');
                    setTimeout(() => { Sound.enabled = false; }, 40);
                  }
                  local.sounds = next;
                  saveLocalState();
                }}
              />
              <span>Enable UI sounds</span>
            </label>
          </div>

          <div class="setting-group">
            <label class="setting-label">Background music</label>
            <label class="toggle-label">
              <input
                type="checkbox"
                id="music-toggle"
                checked={Music.enabled}
                onChange={(e) => {
                  if ((e.currentTarget as HTMLInputElement).checked !== Music.enabled) Music.toggle();
                  Sound.ui(Music.enabled ? 'affirm' : 'soft');
                }}
              />
              <span>Enable background music</span>
            </label>
            <div class="music-volume-row" id="music-volume-row" style={{ display: Music.enabled ? '' : 'none' }}>
              <label class="setting-sublabel">Volume</label>
              <div class="music-volume-control">
                <input
                  type="range"
                  id="music-volume-slider"
                  min="0"
                  max="100"
                  step="1"
                  value={String(Music.volume)}
                  onInput={(e) => Music.setVolume(parseInt((e.currentTarget as HTMLInputElement).value, 10))}
                />
                <span id="music-volume-value" class="memory-value">{Music.volume}%</span>
              </div>
            </div>
          </div>
        </section>

        <section class="settings-section-card" id="settings-section-launch">
          <div class="settings-section-head">
            <span class="settings-section-kicker">Launch</span>
            <h3 class="settings-section-title">Window defaults</h3>
          </div>

          <div class="setting-row">
            <div class="setting-group">
              <label class="setting-label">Window Width</label>
              <input
                type="number"
                id="setting-width"
                class="setting-input"
                placeholder="Default"
                autocomplete="off"
                value={settingsWindowWidth.value}
                onInput={(e) => { settingsWindowWidth.value = (e.currentTarget as HTMLInputElement).value; }}
              />
            </div>
            <div class="setting-group">
              <label class="setting-label">Window Height</label>
              <input
                type="number"
                id="setting-height"
                class="setting-input"
                placeholder="Default"
                autocomplete="off"
                value={settingsWindowHeight.value}
                onInput={(e) => { settingsWindowHeight.value = (e.currentTarget as HTMLInputElement).value; }}
              />
            </div>
          </div>
          <p class="setting-hint">Leave these empty to let Minecraft choose its own size on launch.</p>
        </section>

        <section class="settings-section-card" id="settings-section-java">
          <div class="settings-section-head">
            <span class="settings-section-kicker">Runtime</span>
            <h3 class="settings-section-title">Java selection</h3>
          </div>

          <div class="setting-group">
            <label class="setting-label">Java Path Override</label>
            <input
              type="text"
              id="setting-java-path"
              class="setting-input"
              placeholder="Leave empty for auto-detect"
              autocomplete="off"
              value={settingsJavaPath.value}
              onInput={(e) => { settingsJavaPath.value = (e.currentTarget as HTMLInputElement).value; }}
            />
            <p class="setting-hint">Full path to `javaw.exe` or your Java binary.</p>
          </div>
          <div class="setting-group">
            <label class="setting-label">Detected Java Runtimes</label>
            <div class="java-runtimes" id="java-runtimes">
              {javaRuntimeState === 'loading' && <span class="setting-hint">Loading...</span>}
              {javaRuntimeState === 'error' && <span class="setting-hint">Failed to load</span>}
              {javaRuntimeState === 'ready' && javaRuntimes.length === 0 && <span class="setting-hint">No runtimes detected</span>}
              {javaRuntimeState === 'ready' && javaRuntimes.map((runtime) => (
                <div class="java-runtime-item">
                  <span class="java-runtime-component">{runtime.component}</span>
                  <span class="java-runtime-source">{runtime.source}</span>
                </div>
              ))}
            </div>
          </div>

          <div class="setting-group">
            <label class="setting-label">JVM Performance Preset</label>
            <div class="jvm-preset-group" id="jvm-preset-group">
              {JVM_PRESETS.map((preset) => (
                <label class="radio-label">
                  <input
                    type="radio"
                    name="jvm-preset"
                    value={preset.value}
                    checked={settingsJvmPreset.value === preset.value}
                    onChange={() => { settingsJvmPreset.value = preset.value; }}
                  />
                  {' '}
                  {preset.label}
                  {preset.hint && <span class="setting-hint-inline">{preset.hint}</span>}
                  {preset.tip && <span class="info-tip" data-tip={preset.tip}>i</span>}
                </label>
              ))}
            </div>
          </div>
        </section>

        <section class="settings-section-card" id="settings-section-shortcuts">
          <div class="settings-section-head">
            <span class="settings-section-kicker">Shortcuts</span>
            <h3 class="settings-section-title">Keyboard flow</h3>
          </div>
          <p class="setting-hint">Hold <kbd class="shortcut-key">Ctrl</kbd> anywhere to reveal hints. Click a binding to change it.</p>
          <div class="shortcut-list" id="shortcut-list">
            {Shortcuts.all().map((action) => {
              const binding = Shortcuts.get(action)!;
              const isCustom = !!local.shortcuts[action];
              const isRecording = recordingShortcut.value === action;
              return (
                <div class="shortcut-item" data-sc-action={action}>
                  <button
                    type="button"
                    class={`shortcut-key shortcut-item-key${isRecording ? ' recording' : ''}`}
                    data-sc-record={action}
                    title="Click to change"
                    onClick={() => startRecording(action)}
                  >
                    {isRecording ? 'Press keys...' : Shortcuts.format(action)}
                  </button>
                  <span class="shortcut-desc">
                    {binding.desc}
                    {isCustom && (
                      <button
                        type="button"
                        class="shortcut-item-reset"
                        data-sc-reset={action}
                        onClick={() => resetShortcut(action)}
                      >
                        reset
                      </button>
                    )}
                  </span>
                </div>
              );
            })}
          </div>
        </section>

        <section class={`settings-section-card${isDevMode ? '' : ' hidden'}`} id="settings-section-advanced">
          <div class="settings-section-head">
            <span class="settings-section-kicker">Advanced</span>
            <h3 class="settings-section-title">Maintenance</h3>
          </div>
          <p class="setting-hint">Use cleanup tools carefully. Worlds and mods are backed up before destructive actions.</p>

          <div class={`dev-tools${isDevMode ? '' : ' hidden'}`} id="dev-tools">
            <div class="setting-group">
              <label class="setting-label" style={{ color: 'var(--amber)' }}>Developer Tools</label>
              <div class="dev-actions">
                <button class="btn-danger" id="dev-cleanup">Cleanup All</button>
                <button class="btn-danger" id="dev-flush">Flush All Data</button>
              </div>
            </div>
          </div>
        </section>
      </div>
    </div>
  );
}
