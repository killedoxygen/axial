interface TauriInvokeBinding {
  invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T>;
}

interface TauriEventBinding {
  listen(eventName: string, callback: (event: { payload: any }) => void): Promise<() => void>;
}

interface TauriOpenerBinding {
  openUrl(url: string): Promise<void>;
}

interface TauriBinding {
  core?: TauriInvokeBinding;
  event?: TauriEventBinding;
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
  eligible: boolean;
  token: string | null;
  position: { x: number; y: number } | null;
  error: string | null;
}

export interface NativeMicrosoftSignInResult {
  status: 'authenticated' | 'cancelled';
  login_id?: string | null;
  profile_name?: string | null;
  owns_minecraft_java?: boolean | null;
}

export function isTauriRuntime(): boolean {
  return getTauriBinding() !== null;
}

export function hasNativeDesktopRuntime(): boolean {
  return isTauriRuntime();
}

export type DesktopPlatform = 'browser' | 'linux' | 'macos' | 'unknown' | 'windows';
export type DesktopChromeMode = 'browser' | 'custom-frameless' | 'mac-overlay' | 'native-decorated';

export interface NativeDesktopChrome {
  platform: DesktopPlatform;
  chrome_mode: DesktopChromeMode;
}

const browserDesktopChrome: NativeDesktopChrome = {
  platform: 'browser',
  chrome_mode: 'browser',
};

let desktopChrome = browserDesktopChrome;

function desktopPlatformFromPayload(value: unknown): DesktopPlatform {
  return value === 'linux' || value === 'macos' || value === 'windows' ? value : 'unknown';
}

function desktopChromeModeFromPayload(value: unknown): DesktopChromeMode {
  return value === 'custom-frameless' || value === 'mac-overlay' || value === 'native-decorated' ? value : 'browser';
}

function desktopChromeFromPayload(payload: unknown): NativeDesktopChrome {
  if (!payload || typeof payload !== 'object') return browserDesktopChrome;
  const record = payload as { platform?: unknown; chrome_mode?: unknown };
  return {
    platform: desktopPlatformFromPayload(record.platform),
    chrome_mode: desktopChromeModeFromPayload(record.chrome_mode),
  };
}

async function getNativeDesktopChrome(): Promise<NativeDesktopChrome> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return browserDesktopChrome;
  const payload = await tauri.core.invoke<unknown>('desktop_chrome');
  return desktopChromeFromPayload(payload);
}

export function hasCustomWindowControls(): boolean {
  return desktopChrome.chrome_mode === 'custom-frameless';
}

export function hasCustomDragRegion(): boolean {
  return desktopChrome.chrome_mode === 'custom-frameless' || desktopChrome.chrome_mode === 'mac-overlay';
}

export async function applyDesktopChromeAttributes(): Promise<void> {
  desktopChrome = await getNativeDesktopChrome().catch(() => browserDesktopChrome);
  const root = document.documentElement;
  root.dataset.desktopPlatform = desktopChrome.platform;
  root.dataset.desktopChrome = desktopChrome.chrome_mode;
  root.dataset.windowControls = hasCustomWindowControls() ? 'custom' : 'native';
  root.dataset.windowFrame = desktopChrome.chrome_mode;
}

export function nativeInstallEventName(installId: string): string {
  return `axial:install:${installId}:progress`;
}

export function nativeLoaderInstallEventName(installId: string): string {
  return `axial:loader-install:${installId}:progress`;
}

export function nativeLaunchStatusEventName(sessionId: string): string {
  return `axial:launch:${sessionId}:status`;
}

export function nativeLaunchLogEventName(sessionId: string): string {
  return `axial:launch:${sessionId}:log`;
}

export const nativeDesktopCloseBlockedEventName = 'axial:desktop:close-blocked';

export async function onNativeEvent(
  eventName: string,
  callback: (data: any) => void,
): Promise<{ close(): void } | null> {
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

function nativeDragDropPayload(payload: any): NativeDragDropPayload | null {
  const type = payload?.type;
  if (type !== 'enter' && type !== 'over' && type !== 'drop' && type !== 'leave') return null;
  const x = payload?.position?.x;
  const y = payload?.position?.y;
  const position =
    typeof x === 'number' && typeof y === 'number' && Number.isFinite(x) && Number.isFinite(y) ? { x, y } : null;
  return {
    type,
    eligible: payload?.eligible === true,
    token: typeof payload?.token === 'string' && payload.token ? payload.token : null,
    position,
    error: typeof payload?.error === 'string' && payload.error ? payload.error : null,
  };
}

export async function onNativeDragDrop(
  callback: (payload: NativeDragDropPayload) => void,
): Promise<{ close(): void } | null> {
  const tauri = getTauriBinding();
  if (!tauri?.event) return null;

  const unsubscribe = await tauri.event.listen('axial:desktop:skin-drag', (event) => {
    const payload = nativeDragDropPayload(event.payload);
    if (payload) callback(payload);
  });

  return {
    close(): void {
      unsubscribe();
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

export async function signInWithMicrosoft(): Promise<NativeMicrosoftSignInResult | undefined> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return undefined;
  return tauri.core.invoke<NativeMicrosoftSignInResult>('microsoft_sign_in');
}

export async function requestNativeAppRestart(): Promise<boolean> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return false;
  await tauri.core.invoke('app_restart');
  return true;
}

export async function requestNativeAppReset(): Promise<boolean> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return false;
  await tauri.core.invoke('app_reset');
  return true;
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

  const name = typeof record.name === 'string' && record.name.trim() ? record.name.trim() : 'skin.png';

  return new File([bytes], name, { type: 'image/png' });
}

export async function pickNativeSkinFile(): Promise<File | null | undefined> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return undefined;
  const payload = await tauri.core.invoke<unknown>('pick_skin_file');
  return payload === null ? null : nativeSkinFileFromPayload(payload);
}

export async function consumeNativeSkinDrop(token: string): Promise<File | undefined> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return undefined;

  const payload = await tauri.core.invoke<unknown>('consume_skin_drop', { token });
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
