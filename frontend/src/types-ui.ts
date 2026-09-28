export type { ShortcutBinding } from './generated/ShortcutBinding';
export type { OverlayPosition } from './generated/OverlayPosition';
export type { LocalPreferences as LocalPrefs } from './generated/LocalPreferences';

export type ToastKind = 'success' | 'error' | 'info';

export interface ToastItem {
  id: number;
  message: string;
  type: ToastKind;
}
