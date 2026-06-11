import type { JSX } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { Button, Segmented } from '../../ui/Atoms';
import { ColorField } from './ColorField';
import { applyTheme, resetThemeToDefault } from '../../theme';
import { defaults, local, PRESET_HUES } from '../../state';
import { Sound, playSliderSound } from '../../sound';
import { useTheme } from '../../hooks/use-theme';

export function AccentModeToggle({
  onChange,
}: {
  onChange?: (mode: 'dark' | 'light') => void;
}): JSX.Element {
  const theme = useTheme();
  const mode = theme.dark ? 'dark' : 'light';

  const applyMode = (next: 'dark' | 'light'): void => {
    applyTheme(local.theme || 'custom', local.customHue, {
      vibrancy: local.customVibrancy,
      lightness: next === 'light' ? 60 : 0,
    });
    onChange?.(next);
  };

  return (
    <Segmented<'dark' | 'light'>
      value={mode}
      onChange={applyMode}
      options={[{ value: 'dark', label: 'Dark' }, { value: 'light', label: 'Light' }]}
    />
  );
}

export function AccentField({
  showPresets = true,
}: {
  showPresets?: boolean;
} = {}): JSX.Element {
  const [hue, setHue] = useState<number>(local.customHue);
  const [vibrancy, setVibrancy] = useState<number>(local.customVibrancy);
  const latest = useRef({ hue: local.customHue, vibrancy: local.customVibrancy });
  const previewFrame = useRef<number | null>(null);

  useEffect(() => {
    return () => {
      if (previewFrame.current != null) cancelAnimationFrame(previewFrame.current);
    };
  }, []);

  const schedulePreview = (): void => {
    if (previewFrame.current != null) return;
    previewFrame.current = requestAnimationFrame(() => {
      previewFrame.current = null;
      const { hue: nextHue, vibrancy: nextVibrancy } = latest.current;
      setHue(nextHue);
      setVibrancy(nextVibrancy);
      applyTheme('custom', nextHue, {
        vibrancy: nextVibrancy,
        lightness: local.lightness,
        silent: true,
        transient: true,
      });
    });
  };

  const applyPreset = (id: string): void => {
    const h = PRESET_HUES[id];
    if (h == null) return;
    const nextVibrancy = local.customVibrancy;
    latest.current = { hue: h, vibrancy: nextVibrancy };
    setHue(h);
    setVibrancy(nextVibrancy);
    applyTheme(id, null, { vibrancy: nextVibrancy, lightness: local.lightness });
  };

  const resetToDefault = (): void => {
    latest.current = { hue: defaults.customHue, vibrancy: defaults.customVibrancy };
    setHue(defaults.customHue);
    setVibrancy(defaults.customVibrancy);
    resetThemeToDefault();
  };

  const onDrag = (h: number, v: number): void => {
    latest.current = { hue: h, vibrancy: v };
    playSliderSound(h / 360, 'hue');
    schedulePreview();
  };

  const onEnd = (): void => {
    if (previewFrame.current != null) {
      cancelAnimationFrame(previewFrame.current);
      previewFrame.current = null;
    }
    const { hue: nextHue, vibrancy: nextVibrancy } = latest.current;
    setHue(nextHue);
    setVibrancy(nextVibrancy);
    Sound.ui('theme');
    applyTheme('custom', nextHue, { vibrancy: nextVibrancy, lightness: local.lightness });
  };

  return (
    <div class="cp-accent-pane">
      <div class="cp-accent-field">
        <ColorField hue={hue} vibrancy={vibrancy} onChange={onDrag} onEnd={onEnd} />
      </div>
      <div class="cp-accent-readout">
        <div
          class="cp-accent-chip"
          style={{ background: `oklch(0.78 ${(vibrancy / 100) * 0.14} ${hue})` }}
          aria-hidden="true"
        />
        <div class="cp-accent-readout-labels">
          <span>hue <strong>{hue}°</strong></span>
          <span class="cp-accent-sep" />
          <span>chroma <strong>{vibrancy}%</strong></span>
        </div>
      </div>
      {showPresets && (
        <div class="cp-accent-presets">
          <div class="cp-accent-presets-head">
            <div class="cp-accent-presets-label">Presets</div>
            <Button variant="secondary" size="sm" icon="reload" onClick={resetToDefault}>
              Reset
            </Button>
          </div>
          <div class="cp-swatch-row">
            {Object.entries(PRESET_HUES).map(([id, h]) => {
              const active = local.theme === id;
              return (
                <button
                  key={id}
                  class="cp-swatch"
                  data-active={active}
                  aria-label={id}
                  title={id}
                  style={{ background: `oklch(0.78 0.14 ${h})`, color: `oklch(0.78 0.14 ${h})` }}
                  onClick={() => applyPreset(id)}
                />
              );
            })}
          </div>
        </div>
      )}
    </div>
  );
}
