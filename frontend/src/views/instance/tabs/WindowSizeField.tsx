import type { JSX } from 'preact';
import { Input } from '../../../ui/Atoms';
import { Segmented } from '../../../ui/Segmented';

export type WindowPreset = { id: string; label: string; w: number; h: number };

const MIN_DIM = 320;
const MAX_DIM = 3840;

function clamp(value: number, min: number, max: number): number {
  return Math.max(min, Math.min(max, value));
}

function snapEven(value: number): number {
  return Math.round(value / 2) * 2;
}

export function WindowSizeField({
  width,
  height,
  presets,
  onChange,
}: {
  width: number;
  height: number;
  presets: WindowPreset[];
  onChange: (w: number, h: number) => void;
}): JSX.Element {
  const activePreset = presets.find((preset) => preset.w === width && preset.h === height)?.id ?? 'custom';

  return (
    <div class="cp-winsize">
      <div class="cp-iset-seg" aria-label="Window size preset">
        <Segmented<string>
          value={activePreset}
          onChange={(presetId) => {
            const preset = presets.find((item) => item.id === presetId);
            if (preset) onChange(preset.w, preset.h);
          }}
          options={[
            ...presets.map((preset) => ({ value: preset.id, label: preset.label })),
            ...(activePreset === 'custom' ? [{ value: 'custom', label: 'Custom' }] : []),
          ]}
        />
      </div>
      <div class="cp-settings-dimensions">
        <label>
          <span>Width</span>
          <Input
            type="number"
            value={String(width)}
            onChange={(v) => onChange(clamp(snapEven(Number.parseInt(v, 10) || width), MIN_DIM, MAX_DIM), height)}
          />
        </label>
        <label>
          <span>Height</span>
          <Input
            type="number"
            value={String(height)}
            onChange={(v) => onChange(width, clamp(snapEven(Number.parseInt(v, 10) || height), MIN_DIM, MAX_DIM))}
          />
        </label>
      </div>
    </div>
  );
}
