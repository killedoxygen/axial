import { themeSignal } from '../theme';
import type { Theme } from '../tokens';

export function useTheme(): Theme {
  return themeSignal.value;
}
