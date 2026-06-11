import type { JSX } from 'preact';
import { useRef } from 'preact/hooks';
import { playSliderSound } from '../sound';

export interface SliderZone {
  from: number;
  to: number;
  tone: 'low' | 'sweet' | 'high' | 'extreme';
  label?: string;
}

function clamp(value: number, min: number, max: number): number {
  return Math.max(min, Math.min(max, value));
}

function zoneStyle(zone: SliderZone, min: number, max: number): JSX.CSSProperties {
  const range = Math.max(1e-9, max - min);
  const from = clamp(zone.from, min, max);
  const to = clamp(zone.to, from, max);
  return {
    left: `${((from - min) / range) * 100}%`,
    width: `${((to - from) / range) * 100}%`,
  };
}

function zonesFromRecommendation(recommended: [number, number] | undefined, min: number, max: number): SliderZone[] {
  if (!recommended) return [];
  const low = clamp(recommended[0], min, max);
  const high = clamp(recommended[1], low, max);
  const range = Math.max(1e-9, max - min);
  const extremeFrom = clamp(max - (range * 0.18), high, max);
  const zones: SliderZone[] = [];
  if (low > min) zones.push({ from: min, to: low, tone: 'low', label: 'Low' });
  zones.push({ from: low, to: high, tone: 'sweet', label: 'Sweet spot' });
  if (extremeFrom > high) zones.push({ from: high, to: extremeFrom, tone: 'high', label: 'High' });
  if (extremeFrom < max) zones.push({ from: extremeFrom, to: max, tone: 'extreme', label: 'Extreme' });
  return zones;
}

function snapToStep(value: number, min: number, max: number, step: number): number {
  const safeStep = step > 0 ? step : 1;
  const snapped = Math.round((value - min) / safeStep) * safeStep + min;
  const decimals = Math.max(0, `${safeStep}`.split('.')[1]?.length ?? 0);
  return Number(clamp(snapped, min, max).toFixed(decimals));
}

// Slider wraps a hidden native range input so keyboard and a11y come for free
// Track, fill, recommendation zones, and thumb are painted on top
export function Slider({
  value,
  min = 0,
  max = 100,
  step = 1,
  onChange,
  onCommit,
  recommended,
  zones,
  sound = false,
  soundValue,
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
  zones?: SliderZone[];
  sound?: string | false;
  soundValue?: (v: number) => number;
  ticks?: number[];
  ariaLabel?: string;
  style?: JSX.CSSProperties;
}): JSX.Element {
  const inputRef = useRef<HTMLInputElement | null>(null);
  const range = Math.max(1e-9, max - min);
  const clampedValue = clamp(value, min, max);
  const pct = ((clampedValue - min) / range) * 100;
  const shownZones = (zones ?? zonesFromRecommendation(recommended, min, max))
    .filter(zone => clamp(zone.to, min, max) > clamp(zone.from, min, max));
  const emit = (next: number): void => {
    if (sound) {
      const normalized = soundValue ? soundValue(next) : (clamp(next, min, max) - min) / range;
      playSliderSound(normalized, sound);
    }
    onChange(next);
  };
  const valueFromPointer = (event: PointerEvent, element: HTMLElement): number => {
    const rect = element.getBoundingClientRect();
    const ratio = clamp((event.clientX - rect.left) / Math.max(1, rect.width), 0, 1);
    return snapToStep(min + ratio * range, min, max, step);
  };
  const handlePointer = (event: PointerEvent, commit: boolean): void => {
    const element = event.currentTarget as HTMLElement;
    const next = valueFromPointer(event, element);
    if (inputRef.current) inputRef.current.focus();
    emit(next);
    if (commit) onCommit?.(next);
  };
  return (
    <div>
      <div
        class="cp-slider"
        style={{ ...style, ['--slider-filled' as any]: `${pct}%` }}
        onPointerDown={(event) => {
          if (event.button !== 0) return;
          event.preventDefault();
          (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
          handlePointer(event as unknown as PointerEvent, false);
        }}
        onPointerMove={(event) => {
          if (!(event.currentTarget as HTMLElement).hasPointerCapture(event.pointerId)) return;
          event.preventDefault();
          handlePointer(event as unknown as PointerEvent, false);
        }}
        onPointerUp={(event) => {
          if (!(event.currentTarget as HTMLElement).hasPointerCapture(event.pointerId)) return;
          event.preventDefault();
          handlePointer(event as unknown as PointerEvent, true);
          (event.currentTarget as HTMLElement).releasePointerCapture(event.pointerId);
        }}
      >
        <div class="cp-slider-track">
          {shownZones.map((zone, index) => (
            <div
              key={`${zone.tone}-${index}`}
              class="cp-slider-zone"
              data-tone={zone.tone}
              title={zone.label}
              style={zoneStyle(zone, min, max)}
            />
          ))}
          <div class="cp-slider-fill" />
        </div>
        <input
          ref={inputRef}
          type="range"
          min={min} max={max} step={step} value={value}
          aria-label={ariaLabel}
          onInput={(e: any) => emit(parseFloat(e.currentTarget.value))}
          onChange={(e: any) => onCommit?.(parseFloat(e.currentTarget.value))}
        />
        <div class="cp-slider-thumb" aria-hidden="true" />
      </div>
      {ticks && ticks.length > 0 && (
        <div class="cp-slider-ticks">
          {ticks.map(t => (
            <button key={t} type="button" data-active={value === t} onClick={() => { emit(t); onCommit?.(t); }}>
              {t}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
