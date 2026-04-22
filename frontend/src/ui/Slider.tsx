import type { JSX } from 'preact';
import './slider.css';

// Slider wraps a hidden native range input so keyboard and a11y come for free
// Track, fill, recommended zone, and thumb are painted on top
export function Slider({
  value,
  min = 0,
  max = 100,
  step = 1,
  onChange,
  onCommit,
  recommended,
  ticks,
  ariaLabel,
  style,
}: {
  value: number;
  min?: number;
  max?: number;
  step?: number;
  onChange: (v: number) => void;
  onCommit?: (v: number) => void;
  recommended?: [number, number];
  ticks?: number[];
  ariaLabel?: string;
  style?: JSX.CSSProperties;
}): JSX.Element {
  const range = Math.max(1e-9, max - min);
  const pct = ((Math.max(min, Math.min(max, value)) - min) / range) * 100;
  const recStyle: JSX.CSSProperties | undefined = recommended ? {
    left: `${((recommended[0] - min) / range) * 100}%`,
    width: `${((recommended[1] - recommended[0]) / range) * 100}%`,
  } : undefined;
  return (
    <div>
      <div class="cp-slider" style={{ ...style, ['--slider-filled' as any]: `${pct}%` }}>
        <div class="cp-slider-track">
          {recStyle && <div class="cp-slider-rec" style={recStyle} />}
          <div class="cp-slider-fill" />
        </div>
        <input
          type="range"
          min={min} max={max} step={step} value={value}
          aria-label={ariaLabel}
          onInput={(e: any) => onChange(parseFloat(e.currentTarget.value))}
          onChange={(e: any) => onCommit?.(parseFloat(e.currentTarget.value))}
        />
        <div class="cp-slider-thumb" aria-hidden="true" />
      </div>
      {ticks && ticks.length > 0 && (
        <div class="cp-slider-ticks">
          {ticks.map(t => (
            <button key={t} type="button" data-active={value === t} onClick={() => { onChange(t); onCommit?.(t); }}>
              {t}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
