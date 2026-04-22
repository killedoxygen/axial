// Read the reactive theme, preact re-renders when themeSignal updates
// so components don't need theme passed as a prop
import { themeSignal } from '../theme';
import type { Theme } from '../tokens';

export function useTheme(): Theme {
  return themeSignal.value;
}
