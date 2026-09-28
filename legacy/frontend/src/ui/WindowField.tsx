import type { JSX } from 'preact';
import { useEffect, useMemo, useState } from 'preact/hooks';
import { Input } from './Atoms';
import { Segmented } from './Segmented';
import { buildWindowPresets, detectMaxScreenSize, type ScreenSize, type WindowPresetSpec } from './screen-presets';

const MIN_DIM = 320;
const MAX_DIM = 3840;
const GAME_DEFAULT: WindowPresetSpec = { id: 'default', label: 'Default', w: 854, h: 480 };

function clampDim(value: number): number {
  const even = Math.round(value / 2) * 2;
  return Math.max(MIN_DIM, Math.min(MAX_DIM, even));
}

export function WindowField({
  width,
  height,
  inherit,
  onCommit,
}: {
  width: number;
  height: number;
  inherit?: { active: boolean; label: string };
  onCommit: (w: number, h: number) => void;
}): JSX.Element {
  const [screen, setScreen] = useState<ScreenSize>({
    w: window.screen?.width || 1920,
    h: window.screen?.height || 1080,
  });
  const [wDraft, setWDraft] = useState(String(width));
  const [hDraft, setHDraft] = useState(String(height));

  useEffect(() => {
    let alive = true;
    void detectMaxScreenSize().then((size) => {
      if (alive) setScreen(size);
    });
    return () => {
      alive = false;
    };
  }, []);

  useEffect(() => {
    setWDraft(String(width));
    setHDraft(String(height));
  }, [width, height]);

  const presets = useMemo<WindowPresetSpec[]>(() => {
    const fits = buildWindowPresets(screen)
      .filter((preset) => preset.w > 0)
      .reverse();
    return [GAME_DEFAULT, ...fits];
  }, [screen]);

  const inheriting = inherit?.active === true;
  const activePreset = inheriting ? '' : (presets.find((p) => p.w === width && p.h === height)?.id ?? 'custom');

  const commitDrafts = (): void => {
    const w = clampDim(Number.parseInt(wDraft, 10) || width);
    const h = clampDim(Number.parseInt(hDraft, 10) || height);
    setWDraft(String(w));
    setHDraft(String(h));
    if (w !== width || h !== height) onCommit(w, h);
  };

  const onDimKeyDown = (e: KeyboardEvent): void => {
    if (e.key === 'Enter') (e.currentTarget as HTMLInputElement | null)?.blur();
  };

  const windowPct = Math.max(6, Math.min(100, (width / Math.max(1, screen.w)) * 100));

  return (
    <div class="cp-winfield" data-inherit={inheriting ? 'true' : 'false'}>
      <div class="cp-winfield-stage">
        <div
          class="cp-winfield-screen"
          style={{ aspectRatio: `${screen.w} / ${screen.h}` }}
          title={`Display ${screen.w} × ${screen.h}`}
        >
          <div class="cp-winfield-window" style={{ width: `${windowPct}%`, aspectRatio: `${width} / ${height}` }} />
        </div>
        <span class="cp-winfield-caption">{inheriting ? inherit!.label : `${width} × ${height}`}</span>
      </div>
      <div class="cp-winfield-controls">
        <Segmented<string>
          size="sm"
          ariaLabel="Window size preset"
          value={activePreset}
          onChange={(presetId) => {
            const preset = presets.find((item) => item.id === presetId);
            if (!preset) return;
            if (!inheriting && preset.w === width && preset.h === height) return;
            onCommit(preset.w, preset.h);
          }}
          options={[
            ...presets.map((preset) => ({ value: preset.id, label: preset.label, title: `${preset.w} × ${preset.h}` })),
            ...(activePreset === 'custom' ? [{ value: 'custom', label: 'Custom' }] : []),
          ]}
        />
        <div class="cp-winfield-dims">
          <label>
            <span>Width</span>
            <Input type="number" value={wDraft} onChange={setWDraft} onBlur={commitDrafts} onKeyDown={onDimKeyDown} />
          </label>
          <label>
            <span>Height</span>
            <Input type="number" value={hDraft} onChange={setHDraft} onBlur={commitDrafts} onKeyDown={onDimKeyDown} />
          </label>
        </div>
      </div>
    </div>
  );
}
