import type { ComponentChildren, JSX } from 'preact';
import { RangeSlider } from './Slider';
import { Chip } from './Atoms';
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

/** The smallest heap that can be set; the gauge starts here, so every block on it can be used. */
const FLOOR_GB = 1;

/**
 * The maximum heap as the readout, then one block per GB from the floor up to the ceiling, as the range slider's
 * own track: what is lit follows the handles exactly, so it never lags a handle that is being dragged.
 */
export function MemoryField({
  minGb,
  maxGb,
  totalGb,
  facts,
  onChange,
  onCommit,
}: {
  minGb: number;
  maxGb: number;
  totalGb: number;
  /** Chips that follow the readout, such as how much memory is installed. */
  facts?: ComponentChildren;
  onChange: (minGb: number, maxGb: number) => void;
  onCommit: (minGb: number, maxGb: number) => void;
}): JSX.Element {
  const ceiling = heapCeilingGb(totalGb);
  const recMax = recommendedHeapRange(totalGb)[1];
  const highBound = Math.min(ceiling, Math.max(recMax, ceiling * 0.75));
  const safeMin = Math.min(minGb, maxGb);
  const blocks = Math.max(1, Math.round(ceiling) - FLOOR_GB);
  const zone = (gb: number): 'err' | 'warn' | undefined => (gb > highBound ? 'err' : gb > recMax ? 'warn' : undefined);
  /** The blocks, drawn once per layer: unlit, below the minimum, and lit. The handles' places cut each layer. */
  const strip = (layer: 'off' | 'floor' | 'lit'): JSX.Element => (
    <div class="cp-memfield-blocks" data-layer={layer}>
      {Array.from({ length: blocks }, (_, i) => (
        <i key={i}>
          <b data-zone={zone(FLOOR_GB + i + 0.25)} />
          <b data-zone={zone(FLOOR_GB + i + 0.75)} />
        </i>
      ))}
    </div>
  );

  return (
    <div class="cp-memfield">
      <div class="cp-memfield-readout">
        <strong>{fmtMem(maxGb)}</strong>
        <Chip>Min {fmtMem(safeMin)}</Chip>
        {facts}
      </div>
      <div class="cp-memfield-gauge" style={{ ['--blocks' as string]: blocks }}>
        <RangeSlider
          low={safeMin}
          high={maxGb}
          min={FLOOR_GB}
          max={ceiling}
          step={0.5}
          sound="memory"
          onChange={onChange}
          onCommit={onCommit}
          ariaLabelLow="Minimum heap in gigabytes"
          ariaLabelHigh="Maximum heap in gigabytes"
          track={
            <>
              {strip('off')}
              {strip('floor')}
              {strip('lit')}
            </>
          }
        />
      </div>
    </div>
  );
}
