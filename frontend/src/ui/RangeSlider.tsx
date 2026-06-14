import type { JSX } from 'preact';
import { useRef } from 'preact/hooks';
import type { SliderZone } from './Slider';
import { playSliderSound } from '../sound';

function clamp(value: number, min: number, max: number): number {
  return Math.max(min, Math.min(max, value));
}

function snapToStep(value: number, min: number, max: number, step: number): number {
  const safeStep = step > 0 ? step : 1;
  const snapped = Math.round((value - min) / safeStep) * safeStep + min;
  const decimals = Math.max(0, `${safeStep}`.split('.')[1]?.length ?? 0);
  return Number(clamp(snapped, min, max).toFixed(decimals));
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

export function RangeSlider({
  low,
  high,
  min = 0,
  max = 100,
  step = 1,
  onChange,
  onCommit,
  zones,
  sound = false,
  ariaLabelLow,
  ariaLabelHigh,
}: {
  low: number;
  high: number;
  min?: number;
  max?: number;
  step?: number;
  onChange: (low: number, high: number) => void;
  onCommit?: (low: number, high: number) => void;
  zones?: SliderZone[];
  sound?: string | false;
  ariaLabelLow?: string;
  ariaLabelHigh?: string;
}): JSX.Element {
  const range = Math.max(1e-9, max - min);
  const lowInput = useRef<HTMLInputElement | null>(null);
  const highInput = useRef<HTMLInputElement | null>(null);
  const dragging = useRef<'low' | 'high' | null>(null);
  const safeLow = clamp(low, min, high);
  const safeHigh = clamp(high, safeLow, max);
  const lowPct = ((safeLow - min) / range) * 100;
  const highPct = ((safeHigh - min) / range) * 100;
  const shownZones = (zones ?? []).filter((zone) => clamp(zone.to, min, max) > clamp(zone.from, min, max));

  const emit = (nextLow: number, nextHigh: number, which: 'low' | 'high'): void => {
    if (sound) playSliderSound((clamp(which === 'low' ? nextLow : nextHigh, min, max) - min) / range, sound);
    onChange(nextLow, nextHigh);
  };

  const valueFromPointer = (event: PointerEvent, element: HTMLElement): number => {
    const rect = element.getBoundingClientRect();
    const ratio = clamp((event.clientX - rect.left) / Math.max(1, rect.width), 0, 1);
    return snapToStep(min + ratio * range, min, max, step);
  };

  const applyValue = (raw: number, which: 'low' | 'high', commit: boolean): void => {
    let nextLow = safeLow;
    let nextHigh = safeHigh;
    if (which === 'low') nextLow = clamp(raw, min, safeHigh);
    else nextHigh = clamp(raw, safeLow, max);
    emit(nextLow, nextHigh, which);
    if (commit) onCommit?.(nextLow, nextHigh);
  };

  const handlePointer = (event: PointerEvent, element: HTMLElement, commit: boolean): void => {
    const raw = valueFromPointer(event, element);
    let which = dragging.current;
    if (!which) {
      which = Math.abs(raw - safeLow) <= Math.abs(raw - safeHigh) ? 'low' : 'high';
      if (raw === safeLow && raw === safeHigh) which = raw <= safeLow ? 'low' : 'high';
    }
    (which === 'low' ? lowInput : highInput).current?.focus();
    applyValue(raw, which, commit);
  };

  return (
    <div
      class="cp-slider cp-rslider"
      style={{ ['--lo' as any]: `${lowPct}%`, ['--hi' as any]: `${highPct}%` }}
      onPointerDown={(event) => {
        if (event.button !== 0) return;
        event.preventDefault();
        const element = event.currentTarget as HTMLElement;
        const raw = valueFromPointer(event as unknown as PointerEvent, element);
        dragging.current = Math.abs(raw - safeLow) <= Math.abs(raw - safeHigh) ? 'low' : 'high';
        element.setPointerCapture(event.pointerId);
        handlePointer(event as unknown as PointerEvent, element, false);
      }}
      onPointerMove={(event) => {
        const element = event.currentTarget as HTMLElement;
        if (!element.hasPointerCapture(event.pointerId)) return;
        event.preventDefault();
        handlePointer(event as unknown as PointerEvent, element, false);
      }}
      onPointerUp={(event) => {
        const element = event.currentTarget as HTMLElement;
        if (!element.hasPointerCapture(event.pointerId)) return;
        event.preventDefault();
        handlePointer(event as unknown as PointerEvent, element, true);
        element.releasePointerCapture(event.pointerId);
        dragging.current = null;
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
        <div class="cp-rslider-band" />
      </div>
      <input
        ref={lowInput}
        type="range"
        class="cp-rslider-input"
        min={min}
        max={max}
        step={step}
        value={safeLow}
        aria-label={ariaLabelLow}
        onInput={(e: any) => applyValue(parseFloat(e.currentTarget.value), 'low', false)}
        onChange={(e: any) => applyValue(parseFloat(e.currentTarget.value), 'low', true)}
      />
      <input
        ref={highInput}
        type="range"
        class="cp-rslider-input"
        min={min}
        max={max}
        step={step}
        value={safeHigh}
        aria-label={ariaLabelHigh}
        onInput={(e: any) => applyValue(parseFloat(e.currentTarget.value), 'high', false)}
        onChange={(e: any) => applyValue(parseFloat(e.currentTarget.value), 'high', true)}
      />
      <div class="cp-slider-thumb cp-rslider-thumb cp-rslider-thumb--lo" aria-hidden="true" />
      <div class="cp-slider-thumb cp-rslider-thumb cp-rslider-thumb--hi" aria-hidden="true" />
    </div>
  );
}
