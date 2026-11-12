import type { ComponentChildren, JSX } from 'preact';
import { useRef, useState } from 'preact/hooks';
import { playSliderSound } from '../sound';

export function clamp(value: number, min: number, max: number): number {
  return Math.max(min, Math.min(max, value));
}

export function snapToStep(value: number, min: number, max: number, step: number): number {
  const safeStep = step > 0 ? step : 1;
  const snapped = Math.round((value - min) / safeStep) * safeStep + min;
  const decimals = Math.max(0, `${safeStep}`.split('.')[1]?.length ?? 0);
  return Number(clamp(snapped, min, max).toFixed(decimals));
}

interface Scale {
  min: number;
  max: number;
  step: number;
}

interface Thumb {
  value: number;
  label?: string;
  /** How far this thumb may go, inside the scale. */
  floor: number;
  ceil: number;
}

/**
 * The track every slider is: one thumb or several. A thumb follows the pointer exactly while it is dragged and
 * settles on the nearest step when let go; the value it reports is always a step. Each thumb's place is published
 * as a ratio, `--r0`, `--r1`…, for whatever the track draws. Keys go to a hidden native range input per thumb.
 */
function Track({
  min,
  max,
  step,
  thumbs,
  onChange,
  onCommit,
  sound,
  class: className,
  children,
}: Scale & {
  thumbs: Thumb[];
  onChange: (thumb: number, value: number) => void;
  onCommit: (thumb: number, value: number) => void;
  sound: string | false;
  class?: string;
  children: ComponentChildren;
}): JSX.Element {
  const range = Math.max(1e-9, max - min);
  const ratio = (value: number): number => (clamp(value, min, max) - min) / range;
  const inputs = useRef<(HTMLInputElement | null)[]>([]);
  const [drag, setDrag] = useState<{ thumb: number; at: number } | null>(null);

  const move = (index: number, raw: number, commit: boolean): void => {
    const thumb = thumbs[index]!;
    const next = clamp(snapToStep(raw, min, max, step), thumb.floor, thumb.ceil);
    if (next !== thumb.value) {
      if (sound) playSliderSound(ratio(next), sound);
      onChange(index, next);
    }
    if (commit) onCommit(index, next);
  };

  const pointAt = (event: JSX.TargetedPointerEvent<HTMLElement>): number => {
    const rect = event.currentTarget.getBoundingClientRect();
    return clamp((event.clientX - rect.left) / Math.max(1, rect.width), 0, 1);
  };
  /** The thumb a press takes: the nearest, and of two on the same spot the one that can go the pointer's way. */
  const nearest = (point: number): number =>
    thumbs.reduce((best, thumb, index) => {
      const gap = Math.abs(ratio(thumb.value) - point);
      const bestGap = Math.abs(ratio(thumbs[best]!.value) - point);
      return gap < bestGap || (gap === bestGap && point > ratio(thumb.value)) ? index : best;
    }, 0);

  const place = (thumb: Thumb, index: number): number =>
    drag?.thumb === index ? clamp(drag.at, ratio(thumb.floor), ratio(thumb.ceil)) : ratio(thumb.value);
  const release = (event: JSX.TargetedPointerEvent<HTMLElement>): void => {
    if (!drag) return;
    move(drag.thumb, min + pointAt(event) * range, true);
    setDrag(null);
  };

  return (
    <div
      class={className ? `cp-slider ${className}` : 'cp-slider'}
      data-dragging={drag ? 'true' : undefined}
      style={Object.fromEntries(thumbs.map((thumb, index) => [`--r${index}`, place(thumb, index)]))}
      onPointerDown={(event) => {
        if (event.button !== 0) return;
        event.preventDefault();
        event.currentTarget.setPointerCapture(event.pointerId);
        const at = pointAt(event);
        const thumb = nearest(at);
        inputs.current[thumb]?.focus();
        setDrag({ thumb, at });
        move(thumb, min + at * range, false);
      }}
      onPointerMove={(event) => {
        if (!drag) return;
        const at = pointAt(event);
        setDrag({ thumb: drag.thumb, at });
        move(drag.thumb, min + at * range, false);
      }}
      onPointerUp={release}
      onPointerCancel={release}
    >
      <div class="cp-slider-track">{children}</div>
      {thumbs.map((thumb, index) => (
        <input
          key={index}
          ref={(element) => {
            inputs.current[index] = element;
          }}
          type="range"
          data-thumb={index}
          min={min}
          max={max}
          step={step}
          value={thumb.value}
          aria-label={thumb.label}
          onInput={(event) => move(index, parseFloat(event.currentTarget.value), false)}
          onChange={(event) => move(index, parseFloat(event.currentTarget.value), true)}
        />
      ))}
      {thumbs.map((_, index) => (
        <div key={index} class="cp-slider-thumb" data-thumb={index} style={{ '--at': `var(--r${index})` }} />
      ))}
    </div>
  );
}

type ZoneTone = 'low' | 'sweet' | 'high' | 'extreme';

/** The scale in bands around a recommended span: below it, in it, above it, and the last stretch. */
function zones(
  recommended: [number, number],
  min: number,
  max: number,
): { from: number; to: number; tone: ZoneTone }[] {
  const low = clamp(recommended[0], min, max);
  const high = clamp(recommended[1], low, max);
  const extreme = clamp(max - (max - min) * 0.18, high, max);
  return [
    { from: min, to: low, tone: 'low' as const },
    { from: low, to: high, tone: 'sweet' as const },
    { from: high, to: extreme, tone: 'high' as const },
    { from: extreme, to: max, tone: 'extreme' as const },
  ].filter((zone) => zone.to > zone.from);
}

export function Slider({
  value,
  min = 0,
  max = 100,
  step = 1,
  onChange,
  onCommit,
  recommended,
  sound = false,
  ariaLabel,
  class: className,
}: Partial<Scale> & {
  value: number;
  onChange: (value: number) => void;
  onCommit?: (value: number) => void;
  /** Tints the track around this span. */
  recommended?: [number, number];
  sound?: string | false;
  ariaLabel?: string;
  class?: string;
}): JSX.Element {
  const range = Math.max(1e-9, max - min);
  return (
    <Track
      min={min}
      max={max}
      step={step}
      thumbs={[{ value: clamp(value, min, max), label: ariaLabel, floor: min, ceil: max }]}
      onChange={(_, next) => onChange(next)}
      onCommit={(_, next) => onCommit?.(next)}
      sound={sound}
      class={className}
    >
      {recommended &&
        zones(recommended, min, max).map((zone) => (
          <div
            key={zone.tone}
            class="cp-slider-zone"
            data-tone={zone.tone}
            style={{
              left: `${((zone.from - min) / range) * 100}%`,
              width: `${((zone.to - zone.from) / range) * 100}%`,
            }}
          />
        ))}
      <div class="cp-slider-fill" />
    </Track>
  );
}

/** Two thumbs on one track; neither passes the other. */
export function RangeSlider({
  low,
  high,
  min = 0,
  max = 100,
  step = 1,
  onChange,
  onCommit,
  sound = false,
  ariaLabelLow,
  ariaLabelHigh,
}: Partial<Scale> & {
  low: number;
  high: number;
  onChange: (low: number, high: number) => void;
  onCommit?: (low: number, high: number) => void;
  sound?: string | false;
  ariaLabelLow?: string;
  ariaLabelHigh?: string;
}): JSX.Element {
  const safeLow = clamp(low, min, high);
  const safeHigh = clamp(high, safeLow, max);
  const pair = (thumb: number, value: number): [number, number] => (thumb === 0 ? [value, safeHigh] : [safeLow, value]);
  return (
    <Track
      min={min}
      max={max}
      step={step}
      thumbs={[
        { value: safeLow, label: ariaLabelLow, floor: min, ceil: safeHigh },
        { value: safeHigh, label: ariaLabelHigh, floor: safeLow, ceil: max },
      ]}
      onChange={(thumb, value) => onChange(...pair(thumb, value))}
      onCommit={(thumb, value) => onCommit?.(...pair(thumb, value))}
      sound={sound}
      class="cp-slider--range"
    >
      <div class="cp-slider-band" />
    </Track>
  );
}
