import type { RefObject } from 'preact';
import { useLayoutEffect } from 'preact/hooks';

const TILE_MIN = 84;
const TILE_MAX = 176;
const COLUMNS = [3, 4, 5, 6, 7, 8];
/** What the stage keeps beside the palette. */
const STAGE_MIN = 320;

/**
 * Fits the palette to the body's height so the page ends on its bottom edge: of the column counts whose tiles stay
 * between 84 and 176px and leave the stage its room, the one whose last row ends nearest that edge without passing
 * it. Written to `--palette-columns` and `--palette-tile` on the body, which never grows with the palette.
 */
export function usePaletteFit(bodyRef: RefObject<HTMLElement>, tileCount: number): void {
  useLayoutEffect(() => {
    const body = bodyRef.current;
    const palette = body?.firstElementChild as HTMLElement | null;
    if (!body || !palette) return undefined;

    const fit = (): void => {
      const grids = [...palette.querySelectorAll<HTMLElement>(':scope > .cp-wear-grid')];
      const tile = grids[0]?.querySelector<HTMLElement>('.cp-wear');
      const holder = tile?.firstElementChild as HTMLElement | null;
      if (!tile || !holder) return;

      const gap = parseFloat(getComputedStyle(grids[0]!).rowGap) || 0;
      const label = tile.offsetHeight - holder.offsetHeight;
      const counts = grids.map((grid) => grid.querySelectorAll(':scope > .cp-wear').length);
      // Everything above the first grid, and the steps between grids, stay as they are.
      const above = grids[0]!.offsetTop - palette.offsetTop;
      const steps = grids.slice(1).reduce((sum, grid) => sum + (parseFloat(getComputedStyle(grid).marginTop) || 0), 0);
      const room = body.clientHeight;
      const width = body.clientWidth - (parseFloat(getComputedStyle(body).columnGap) || 0) - STAGE_MIN;

      let best = { columns: 4, tile: TILE_MIN, used: 0 };
      for (const columns of COLUMNS) {
        const rows = counts.map((count) => Math.ceil(count / columns));
        const total = rows.reduce((sum, count) => sum + count, 0);
        const fixed =
          above + steps + total * label + rows.reduce((sum, count) => sum + Math.max(0, count - 1) * gap, 0);
        const side = Math.min(TILE_MAX, Math.floor((room - fixed) / total));
        if (side < TILE_MIN || columns * side + (columns - 1) * gap > width) continue;
        const used = fixed + total * side;
        if (used > best.used) best = { columns, tile: side, used };
      }
      body.style.setProperty('--palette-columns', String(best.columns));
      body.style.setProperty('--palette-tile', `${best.tile}px`);
    };

    fit();
    const observer = new ResizeObserver(fit);
    observer.observe(body);
    return () => observer.disconnect();
  }, [tileCount]);
}
