import { Sound } from './sound';
import { setPage } from './utils';
import { selectInstance as selectInstanceAction } from './actions';

export function selectInstance(inst: { id: string } | null, options: { silent?: boolean } = {}): void {
  const { silent = false } = options;
  if (!silent) {
    Sound.init();
    Sound.tick();
  }
  selectInstanceAction(inst?.id ?? null);
  setPage('launcher');
}
