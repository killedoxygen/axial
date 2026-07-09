export type Page = 'launcher' | 'settings';

export type SidebarFilter = 'all' | 'release' | 'snapshot' | 'modded';

export interface ShortcutBinding {
  key: string;
  ctrl?: boolean;
  shift?: boolean;
  alt?: boolean;
  meta?: boolean;
}

export interface OverlayPosition {
  x: number;
  y: number;
  scaleX?: number;
  scaleY?: number;
}

export interface LocalPrefs {
  theme: string;
  customHue: number;
  customVibrancy: number;
  lightness: number;
  logHeight: number;
  collapsedGroups: Record<string, boolean>;
  sidebarFilter: string;
  sounds: boolean;
  hideSkinNametag: boolean;
  selectedSkin: string;
  selectedSkinsByAccount: Record<string, string>;
  shortcuts: Record<string, ShortcutBinding>;
  overlayPositions: Record<string, OverlayPosition>;
  lastUpdateCheckAt: string;
  dismissedUpdateVersion: string;
}

export type ToastKind = 'success' | 'error' | 'info';

export interface ToastItem {
  id: number;
  message: string;
  type: ToastKind;
}
