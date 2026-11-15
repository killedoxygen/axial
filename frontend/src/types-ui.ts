export type { ShortcutBinding } from './generated/ShortcutBinding';
export type { OverlayPosition } from './generated/OverlayPosition';
export type { LocalPreferences as LocalPrefs } from './generated/LocalPreferences';

export type ToastKind = 'success' | 'error' | 'info';

export interface ToastAction {
  label: string;
  run: () => void;
  /** The one the notice is really offering; the rest stay quiet. */
  primary?: boolean;
  title?: string;
}

export interface ToastItem {
  id: number;
  message: string;
  type: ToastKind;
  /** A second, quieter line under the message. */
  detail?: string;
  /** What the user can do about it, there and then. */
  actions?: ToastAction[];
}
