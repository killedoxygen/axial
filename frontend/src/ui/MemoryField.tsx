import type { ComponentChildren, JSX } from 'preact';
import { RangeSlider } from './RangeSlider';
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

type Cell = 'base' | 'on' | 'off' | 'warn' | 'warn-off' | 'err' | 'err-off';

/**
 * The maximum heap as the readout, then one block per GB up to the ceiling. The range slider lies over the blocks
 * from the end of the first to the end of the last, so its two handles stand on block boundaries.
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
  const blocks = Math.round(ceiling);
  const cell = (gb: number): Cell => {
    if (gb < safeMin) return 'base';
    const zone = gb > highBound ? 'err' : gb > recMax ? 'warn' : '';
    if (gb < maxGb) return zone || 'on';
    return zone ? `${zone}-off` : 'off';
  };

  return (
    <div class="cp-memfield">
      <div class="cp-memfield-readout">
        <strong>{fmtMem(maxGb)}</strong>
        <Chip>Min {fmtMem(safeMin)}</Chip>
        {facts}
      </div>
      <div class="cp-memfield-gauge" style={{ ['--blocks' as string]: blocks }}>
        <div class="cp-memfield-blocks" aria-hidden="true">
          {Array.from({ length: blocks }, (_, i) => (
            <i key={i}>
              <b data-cell={cell(i + 0.25)} />
              <b data-cell={cell(i + 0.75)} />
            </i>
          ))}
        </div>
        <RangeSlider
          low={safeMin}
          high={maxGb}
          min={1}
          max={ceiling}
          step={0.5}
          sound="memory"
          onChange={onChange}
          onCommit={onCommit}
          ariaLabelLow="Minimum heap in gigabytes"
          ariaLabelHigh="Maximum heap in gigabytes"
        />
      </div>
    </div>
  );
}
