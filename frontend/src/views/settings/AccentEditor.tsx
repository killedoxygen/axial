import type { JSX } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { Button } from '../../ui/Atoms';
import { Slider } from '../../ui/Slider';
import { ThemeTiles } from '../../ui/SettingsSheet';
import { applyTheme, resetThemeToDefault } from '../../theme';
import { defaults, local, PRESET_HUES } from '../../state';
import { Sound } from '../../sound';
import { useTheme } from '../../hooks/use-theme';

type Mode = 'dark' | 'light';

export function ThemeChoice(): JSX.Element {
  const mode: Mode = useTheme().dark ? 'dark' : 'light';
  return (
    <ThemeTiles<Mode>
      value={mode}
      ariaLabel="Theme mode"
      onChange={(next) =>
        applyTheme(local.theme || 'custom', local.customHue, {
          vibrancy: local.customVibrancy,
          lightness: next === 'light' ? 60 : 0,
        })
      }
      options={[
        { value: 'dark', label: 'Dark' },
        { value: 'light', label: 'Light', disabledReason: 'In development' },
      ]}
    />
  );
}

export function AccentField({
  compact = false,
}: {
  /** Without Reset, for onboarding. */
  compact?: boolean;
} = {}): JSX.Element {
  const initialHue = PRESET_HUES[local.theme] ?? local.customHue;
  const [hue, setHue] = useState<number>(initialHue);
  const latest = useRef(initialHue);
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
      setHue(latest.current);
      applyTheme('custom', latest.current, {
        vibrancy: local.customVibrancy,
        lightness: local.lightness,
        silent: true,
        transient: true,
      });
    });
  };

  const applyPreset = (id: string): void => {
    const h = PRESET_HUES[id];
    if (h == null) return;
    latest.current = h;
    setHue(h);
    applyTheme(id, null, { vibrancy: local.customVibrancy, lightness: local.lightness });
  };

  const resetToDefault = (): void => {
    latest.current = defaults.customHue;
    setHue(defaults.customHue);
    resetThemeToDefault();
  };

  const onDrag = (h: number): void => {
    latest.current = h;
    schedulePreview();
  };

  const onEnd = (): void => {
    if (previewFrame.current != null) {
      cancelAnimationFrame(previewFrame.current);
      previewFrame.current = null;
    }
    setHue(latest.current);
    Sound.ui('theme');
    applyTheme('custom', latest.current, { vibrancy: local.customVibrancy, lightness: local.lightness });
  };

  return (
    <div class="cp-accent">
      <div class="cp-accent-presets" role="group" aria-label="Accent presets">
        {Object.entries(PRESET_HUES).map(([id, h]) => (
          <button
            key={id}
            type="button"
            class="cp-swatch"
            data-active={local.theme === id}
            aria-label={id}
            title={id}
            style={{ ['--hue' as string]: h }}
            onClick={() => applyPreset(id)}
          />
        ))}
      </div>
      <Slider
        class="cp-hue"
        value={hue}
        min={0}
        max={360}
        sound="hue"
        ariaLabel="Accent hue"
        onChange={onDrag}
        onCommit={onEnd}
      />
      {!compact && (
        <Button variant="ghost" onClick={resetToDefault}>
          Reset
        </Button>
      )}
    </div>
  );
}
