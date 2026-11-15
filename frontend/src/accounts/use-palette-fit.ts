import type { RefObject } from 'preact';
import { useLayoutEffect, useState } from 'preact/hooks';

const TILE_MIN = 84;
const TILE_MAX = 176;
const COLUMNS = [3, 4, 5, 6, 7, 8];
/** What the stage keeps beside the palette. */
const STAGE_MIN = 320;

/**
 * Fits the palette to the body's height so the page ends on its bottom edge and never scrolls: of the column counts
 * whose tiles stay between 84 and 176px and leave the stage its room, the one whose last row ends nearest that edge
 * without passing it. When every tile cannot fit even at 84px, the grids give up rows (the first keeps its rows
 * longest, the others keep one) and return how many tiles each may show; the rest go behind "+N". Writes
 * `--palette-columns` and `--palette-tile` on the body, which never grows with the palette.
 */
export function usePaletteFit(bodyRef: RefObject<HTMLElement>, counts: number[]): number[] {
  const [limits, setLimits] = useState<number[]>(() => counts.map(() => Infinity));
  const key = counts.join(',');

  useLayoutEffect(() => {
    const body = bodyRef.current;
    const palette = body?.firstElementChild as HTMLElement | null;
    if (!body || !palette) return undefined;

    const fit = (): void => {
      const grids = [...palette.querySelectorAll<HTMLElement>(':scope > .cp-wear-grid')];
      const tile = grids[0]?.querySelector<HTMLElement>('.cp-wear');
      const holder = tile?.firstElementChild as HTMLElement | null;
      if (!tile || !holder) return;

      const sizes = counts.filter((count) => count > 0);
      const gap = parseFloat(getComputedStyle(grids[0]!).rowGap) || 0;
      const label = tile.offsetHeight - holder.offsetHeight;
      // Everything above the first grid, and the steps between grids, stay as they are.
      const above = grids[0]!.offsetTop - palette.offsetTop;
      const steps = grids.slice(1).reduce((sum, grid) => sum + (parseFloat(getComputedStyle(grid).marginTop) || 0), 0);
      const room = body.clientHeight;
      const width = body.clientWidth - (parseFloat(getComputedStyle(body).columnGap) || 0) - STAGE_MIN;

      const sideFor = (rows: number[]): number => {
        const total = rows.reduce((sum, count) => sum + count, 0);
        const fixed =
          above + steps + rows.reduce((sum, count) => sum + count * label + Math.max(0, count - 1) * gap, 0);
        return Math.min(TILE_MAX, Math.floor((room - fixed) / total));
      };
      // Rows for a total budget: the later grids keep at least one, the first takes what is left.
      const allot = (full: number[], budget: number): number[] => {
        const rest = full.slice(1).map((rows) => Math.min(rows, 1));
        const first = Math.max(1, Math.min(full[0]!, budget - rest.reduce((sum, rows) => sum + rows, 0)));
        let spare = budget - first - rest.reduce((sum, rows) => sum + rows, 0);
        return [
          first,
          ...rest.map((rows, index) => {
            const more = Math.min(full[index + 1]! - rows, Math.max(0, spare));
            spare -= more;
            return rows + more;
          }),
        ];
      };

      let best: { columns: number; tile: number; score: number; rows: number[] } | null = null;
      for (const columns of COLUMNS) {
        if (columns * TILE_MIN + (columns - 1) * gap > width) continue;
        const full = sizes.map((count) => Math.ceil(count / columns));
        const most = full.reduce((sum, rows) => sum + rows, 0);
        for (let budget = most; budget >= full.length; budget -= 1) {
          const rows = budget === most ? full : allot(full, budget);
          const side = Math.min(sideFor(rows), Math.floor((width - (columns - 1) * gap) / columns));
          if (side < TILE_MIN) continue;
          const shown = rows.reduce((sum, count, index) => sum + Math.min(sizes[index]!, count * columns), 0);
          // Everything shown beats any stacking; then the most tiles; then the fullest height.
          const used = rows.reduce((sum, count) => sum + count * (side + label + gap), 0);
          const score = (budget === most ? 1e9 : 0) + shown * 1e4 + used;
          if (!best || score > best.score) best = { columns, tile: side, score, rows };
          break;
        }
      }
      const chosen = best ?? { columns: COLUMNS[0]!, tile: TILE_MIN, rows: sizes.map(() => 1) };
      body.style.setProperty('--palette-columns', String(chosen.columns));
      body.style.setProperty('--palette-tile', `${chosen.tile}px`);

      let index = 0;
      const next = counts.map((count) => {
        if (count === 0) return Infinity;
        const capacity = chosen.rows[index++]! * chosen.columns;
        return count > capacity ? capacity : Infinity;
      });
      setLimits((current) => (current.join(',') === next.join(',') ? current : next));
    };

    fit();
    const observer = new ResizeObserver(fit);
    observer.observe(body);
    return () => observer.disconnect();
  }, [key]);

  return limits;
}
