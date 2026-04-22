import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Segmented } from '../../ui/Atoms';
import { ColorField } from './ColorField';
import { applyTheme } from '../../theme';
import { local, PRESET_HUES } from '../../state';
import { Sound, playSliderSound } from '../../sound';
import './accent.css';

export function AccentModeToggle({
  onChange,
}: {
  onChange?: (mode: 'dark' | 'light') => void;
}): JSX.Element {
  const [mode, setMode] = useState<'dark' | 'light'>(local.lightness >= 50 ? 'light' : 'dark');

  useEffect(() => { setMode(local.lightness >= 50 ? 'light' : 'dark'); }, []);

  const applyMode = (next: 'dark' | 'light'): void => {
    setMode(next);
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

  const applyPreset = (id: string): void => {
    const h = PRESET_HUES[id];
    if (h == null) return;
    setHue(h);
    applyTheme(id, null, { vibrancy, lightness: local.lightness });
  };

  const onDrag = (h: number, v: number): void => {
    setHue(h);
    setVibrancy(v);
    playSliderSound(h / 360, 'hue');
    applyTheme('custom', h, { vibrancy: v, lightness: local.lightness, silent: true });
  };

  const onEnd = (): void => {
    Sound.ui('theme');
    applyTheme('custom', hue, { vibrancy, lightness: local.lightness });
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
          <div class="cp-accent-presets-label">Presets</div>
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
