import type { JSX } from 'preact';
import { RangeSlider } from './RangeSlider';
import type { SliderZone } from './Slider';
import { fmtMem } from '../format';
import { getMemoryRecommendation } from '../utils';

export function heapCeilingGb(totalGb: number): number {
  return Math.max(2, Math.min(32, totalGb));
}

export function recommendedHeapRange(totalGb: number): [number, number] {
  const ceiling = heapCeilingGb(totalGb);
  const rec = getMemoryRecommendation(totalGb);
  const recMax = Math.min(ceiling, rec.rec + 2);
  return [Math.min(recMax, Math.max(1, rec.rec - 2)), recMax];
}

export function MemoryField({
  minGb,
  maxGb,
  totalGb,
  onChange,
  onCommit,
}: {
  minGb: number;
  maxGb: number;
  totalGb: number;
  onChange: (minGb: number, maxGb: number) => void;
  onCommit: (minGb: number, maxGb: number) => void;
}): JSX.Element {
  const ceiling = heapCeilingGb(totalGb);
  const [recMin, recMax] = recommendedHeapRange(totalGb);
  const highBound = Math.min(ceiling, Math.max(recMax, ceiling * 0.75));
  const zones: SliderZone[] = [
    { from: 1, to: recMin, tone: 'low', label: 'Low' },
    { from: recMin, to: recMax, tone: 'sweet', label: 'Recommended' },
    { from: recMax, to: highBound, tone: 'high', label: 'High' },
    { from: highBound, to: ceiling, tone: 'extreme', label: 'Aggressive' },
  ];
  const safeMin = Math.min(minGb, maxGb);

  return (
    <div class="cp-memfield">
      <div class="cp-memfield-readout">
        <span>
          Min <strong>{fmtMem(safeMin)}</strong>
        </span>
        <span class="cp-memfield-band">{fmtMem(maxGb - safeMin)} elastic</span>
        <span>
          Max <strong>{fmtMem(maxGb)}</strong>
        </span>
      </div>
      <RangeSlider
        low={safeMin}
        high={maxGb}
        min={1}
        max={ceiling}
        step={0.5}
        zones={zones}
        sound="memory"
        onChange={onChange}
        onCommit={onCommit}
        ariaLabelLow="Minimum heap in gigabytes"
        ariaLabelHigh="Maximum heap in gigabytes"
      />
    </div>
  );
}
