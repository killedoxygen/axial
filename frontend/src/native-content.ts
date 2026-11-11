import { isDtoRecord } from './dto-contract';
import type { NativeDragDropPayload, NativeDragDropType } from './native';

// Bindings exist only in the desktop app; see native.ts for the shared shell bridge.
const tauri = (): Window['__TAURI__'] | null => (typeof window === 'undefined' ? null : (window.__TAURI__ ?? null));

function nativeError(reason: unknown, fallback: string): never {
  throw new Error(typeof reason === 'string' && reason.trim() ? reason : fallback);
}

export type NativeContentKind = 'mod' | 'modpack';

export interface NativeContentSelection {
  token: string;
  kind: NativeContentKind;
  names: string[];
}

export interface NativeContentDragPayload {
  type: NativeDragDropType;
  /** The kind the dragged files would add, or null when they are not addable. */
  kind: NativeContentKind | null;
  selection: NativeContentSelection | null;
  position: { x: number; y: number } | null;
  error: string | null;
}

function nativeContentKind(value: unknown): NativeContentKind | null {
  return value === 'mod' || value === 'modpack' ? value : null;
}

function nativeContentSelection(value: unknown): NativeContentSelection | null {
  if (!isDtoRecord(value)) return null;
  const kind = nativeContentKind(value.kind);
  const names = Array.isArray(value.names)
    ? value.names.filter((name): name is string => typeof name === 'string')
    : [];
  if (typeof value.token !== 'string' || !kind || names.length === 0) return null;
  return { token: value.token, kind, names };
}

/** Read every file of one native selection; each is handed over exactly once. */
export async function readNativeContentSelection(selection: NativeContentSelection): Promise<File[]> {
  const core = tauri()?.core;
  if (!core) return [];
  const files: File[] = [];
  for (const [index, name] of selection.names.entries()) {
    const bytes = await core
      .invoke('take_content_file', { token: selection.token, index })
      .catch((reason: unknown) => nativeError(reason, `Could not read ${name}.`));
    if (!(bytes instanceof ArrayBuffer)) throw new Error(`Could not read ${name}.`);
    files.push(new File([bytes], name));
  }
  return files;
}

/** `undefined` outside the desktop app, `null` when the picker was dismissed. */
export async function pickNativeContentFiles(kind: NativeContentKind): Promise<File[] | null | undefined> {
  const core = tauri()?.core;
  if (!core) return undefined;
  const payload = await core
    .invoke('pick_content_files', { kind })
    .catch((reason: unknown) => nativeError(reason, 'Could not read the selected files.'));
  if (payload === null) return null;
  const selection = nativeContentSelection(payload);
  if (!selection) throw new Error('The file picker returned an invalid selection.');
  return readNativeContentSelection(selection);
}

export async function onNativeContentDragDrop(
  callback: (payload: NativeContentDragPayload) => void,
): Promise<{ close(): void } | null> {
  const events = tauri()?.event;
  if (!events) return null;
  const unsubscribe = await events.listen('axial:desktop:content-drag', (event) => {
    const record = event.payload;
    if (!isDtoRecord(record)) return;
    const type = record.type;
    if (type !== 'enter' && type !== 'over' && type !== 'drop' && type !== 'leave') return;
    const position = isDtoRecord(record.position) ? record.position : null;
    callback({
      type,
      kind: nativeContentKind(record.kind),
      selection: nativeContentSelection(record.selection),
      position:
        position && Number.isFinite(position.x) && Number.isFinite(position.y)
          ? { x: Number(position.x), y: Number(position.y) }
          : null,
      error: typeof record.error === 'string' && record.error ? record.error : null,
    });
  });
  return {
    close(): void {
      unsubscribe();
    },
  };
}

// Native drag positions are physical pixels on high-density displays; the skin
// drop surface applies the same conversion.
function cssPointFromNativeDrag(position: NativeDragDropPayload['position']): { x: number; y: number } | null {
  if (!position) return null;
  const pixelRatio = window.devicePixelRatio || 1;
  if (pixelRatio > 1 && (position.x > window.innerWidth + 1 || position.y > window.innerHeight + 1)) {
    return {
      x: position.x / pixelRatio,
      y: position.y / pixelRatio,
    };
  }
  return position;
}

export function nativeDragPositionHitsElement(
  position: NativeDragDropPayload['position'],
  element: HTMLElement | null,
): boolean {
  const point = cssPointFromNativeDrag(position);
  if (!point || !element) return false;
  const rect = element.getBoundingClientRect();
  return point.x >= rect.left && point.x <= rect.right && point.y >= rect.top && point.y <= rect.bottom;
}

/** The nearest element matching `selector` under a native drag, wherever the pointer events went. */
export function nativeDragTargetElement<T extends HTMLElement>(
  position: NativeDragDropPayload['position'],
  selector: string,
): T | null {
  const point = cssPointFromNativeDrag(position);
  if (!point) return null;

  for (const element of document.elementsFromPoint(point.x, point.y)) {
    if (!(element instanceof HTMLElement)) continue;
    const match = element.closest(selector);
    if (match instanceof HTMLElement) return match as T;
  }

  for (const element of Array.from(document.querySelectorAll<HTMLElement>(selector))) {
    const rect = element.getBoundingClientRect();
    if (point.x >= rect.left && point.x <= rect.right && point.y >= rect.top && point.y <= rect.bottom) {
      return element as T;
    }
  }

  return null;
}
