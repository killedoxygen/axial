interface TauriInvokeBinding {
  invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T>;
}

interface TauriEventBinding {
  listen(eventName: string, callback: (event: { payload: any }) => void): Promise<() => void>;
}

interface TauriDialogBinding {
  open(options?: Record<string, unknown>): Promise<string | string[] | null>;
  message(message: string, options?: Record<string, unknown>): Promise<void>;
}

interface TauriOpenerBinding {
  openUrl(url: string): Promise<void>;
}

interface TauriBinding {
  core?: TauriInvokeBinding;
  event?: TauriEventBinding;
  dialog?: TauriDialogBinding;
  opener?: TauriOpenerBinding;
}

declare global {
  interface Window {
    __TAURI__?: TauriBinding;
  }
}

function getTauriBinding(): TauriBinding | null {
  return window.__TAURI__ ?? null;
}

export type NativeDragDropType = 'enter' | 'over' | 'drop' | 'leave';

export interface NativeDragDropPayload {
  type: NativeDragDropType;
  paths: string[];
  position: { x: number; y: number } | null;
}

export function isTauriRuntime(): boolean {
  return getTauriBinding() !== null;
}

export function hasNativeDesktopRuntime(): boolean {
  return isTauriRuntime();
}

export function nativeInstallEventName(installId: string): string {
  return `croopor:install:${installId}:progress`;
}

export function nativeLoaderInstallEventName(installId: string): string {
  return `croopor:loader-install:${installId}:progress`;
}

export function nativeLaunchStatusEventName(sessionId: string): string {
  return `croopor:launch:${sessionId}:status`;
}

export function nativeLaunchLogEventName(sessionId: string): string {
  return `croopor:launch:${sessionId}:log`;
}

export async function onNativeEvent(eventName: string, callback: (data: any) => void): Promise<{ close(): void } | null> {
  const tauri = getTauriBinding();
  if (!tauri?.event) return null;

  const unsubscribe = await tauri.event.listen(eventName, (event) => {
    callback(event.payload);
  });

  return {
    close(): void {
      unsubscribe();
    },
  };
}

function nativeDragDropPayload(type: NativeDragDropType, payload: any): NativeDragDropPayload {
  const paths = Array.isArray(payload?.paths)
    ? payload.paths.filter((path: unknown): path is string => typeof path === 'string')
    : [];
  const x = payload?.position?.x;
  const y = payload?.position?.y;
  const position = typeof x === 'number' && typeof y === 'number' && Number.isFinite(x) && Number.isFinite(y)
    ? { x, y }
    : null;
  return { type, paths, position };
}

export async function onNativeDragDrop(
  callback: (payload: NativeDragDropPayload) => void,
): Promise<{ close(): void } | null> {
  const tauri = getTauriBinding();
  if (!tauri?.event) return null;

  const unsubscribe = await Promise.all([
    tauri.event.listen('tauri://drag-enter', (event) => callback(nativeDragDropPayload('enter', event.payload))),
    tauri.event.listen('tauri://drag-over', (event) => callback(nativeDragDropPayload('over', event.payload))),
    tauri.event.listen('tauri://drag-drop', (event) => callback(nativeDragDropPayload('drop', event.payload))),
    tauri.event.listen('tauri://drag-leave', (event) => callback(nativeDragDropPayload('leave', event.payload))),
  ]);

  return {
    close(): void {
      unsubscribe.forEach((close) => close());
    },
  };
}

export async function getNativeAppVersion(): Promise<string | null> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return null;
  return tauri.core.invoke<string>('app_version');
}

export async function getNativeApiBaseUrl(): Promise<string | null> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return null;
  return tauri.core.invoke<string>('api_base_url');
}

export async function requestNativeAppRestart(): Promise<boolean> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return false;
  await tauri.core.invoke('app_restart');
  return true;
}

export async function browseDirectory(defaultPath = ''): Promise<string | null> {
  const tauri = getTauriBinding();
  if (!tauri?.dialog) return null;
  const result = await tauri.dialog.open({
    directory: true,
    defaultPath,
    multiple: false,
  });

  if (Array.isArray(result)) return result[0] ?? null;
  return result;
}

function nativeSkinFileFromPayload(payload: unknown): File {
  if (!payload || typeof payload !== 'object') {
    throw new Error('Native skin picker returned an invalid file.');
  }

  const record = payload as { name?: unknown; bytes?: unknown };
  if (!Array.isArray(record.bytes)) {
    throw new Error('Native skin picker returned an invalid file.');
  }

  const bytes = new Uint8Array(record.bytes.length);
  record.bytes.forEach((value, index) => {
    if (!Number.isInteger(value) || value < 0 || value > 255) {
      throw new Error('Native skin picker returned an invalid file.');
    }
    bytes[index] = value;
  });

  const name = typeof record.name === 'string' && record.name.trim()
    ? record.name.trim()
    : 'skin.png';

  return new File([bytes], name, { type: 'image/png' });
}

export async function pickNativeSkinFile(): Promise<File | null | undefined> {
  const tauri = getTauriBinding();
  if (!tauri?.dialog || !tauri.core) return undefined;

  const result = await tauri.dialog.open({
    directory: false,
    multiple: false,
    filters: [{ name: 'PNG skin', extensions: ['png'] }],
  });

  const path = Array.isArray(result) ? result[0] : result;
  if (!path) return null;

  return readNativeSkinFile(path);
}

export async function readNativeSkinFile(path: string): Promise<File | undefined> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return undefined;

  const payload = await tauri.core.invoke<unknown>('read_skin_file', { path });
  return nativeSkinFileFromPayload(payload);
}

export async function openExternalURL(url: string): Promise<void> {
  let externalUrl: URL;
  try {
    externalUrl = new URL(url);
  } catch {
    throw new Error('External URL must be an absolute HTTPS URL');
  }

  if (externalUrl.protocol !== 'https:') {
    throw new Error('External URL must be an absolute HTTPS URL');
  }

  const tauri = getTauriBinding();
  if (tauri?.opener) {
    await tauri.opener.openUrl(externalUrl.href);
    return;
  }

  window.open(externalUrl.href, '_blank', 'noopener,noreferrer');
}

export async function showNativeNotice(title: string, message: string): Promise<boolean> {
  const tauri = getTauriBinding();
  if (!tauri?.dialog) return false;
  await tauri.dialog.message(message, { title, kind: 'info' });
  return true;
}

export async function startNativeInstallEvents(installId: string): Promise<boolean> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return false;
  await tauri.core.invoke('start_install_events', { installId });
  return true;
}

export async function startNativeLoaderInstallEvents(installId: string): Promise<boolean> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return false;
  await tauri.core.invoke('start_loader_install_events', { installId });
  return true;
}

export async function startNativeLaunchEvents(sessionId: string): Promise<boolean> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return false;
  await tauri.core.invoke('start_launch_events', { sessionId });
  return true;
}

// ── Window controls (Tauri only). In browser mode these are no-ops. ──

export async function windowMinimize(): Promise<boolean> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return false;
  await tauri.core.invoke('window_minimize');
  return true;
}

export async function windowToggleMaximize(): Promise<boolean | null> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return null;
  return tauri.core.invoke<boolean>('window_toggle_maximize');
}

export async function windowClose(): Promise<boolean> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return false;
  await tauri.core.invoke('window_close');
  return true;
}

export async function windowIsMaximized(): Promise<boolean> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return false;
  return tauri.core.invoke<boolean>('window_is_maximized');
}

export async function windowStartDragging(): Promise<boolean> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return false;
  await tauri.core.invoke('window_start_dragging');
  return true;
}

export async function windowSetResizeBackground(dark: boolean): Promise<boolean> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return false;
  await tauri.core.invoke('window_set_resize_background', { dark });
  return true;
}
