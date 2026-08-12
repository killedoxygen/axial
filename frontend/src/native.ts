import { dtoRecord, dtoString, isDtoRecord } from './dto-contract';

interface TauriInvokeBinding {
  invoke(cmd: string, args?: Record<string, unknown>): Promise<unknown>;
}

interface TauriEventBinding {
  listen(eventName: string, callback: (event: { payload: unknown }) => void): Promise<() => void>;
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
  if (typeof window === 'undefined') return null;
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
  if (!isDtoRecord(payload)) return browserDesktopChrome;
  return {
    platform: desktopPlatformFromPayload(payload.platform),
    chrome_mode: desktopChromeModeFromPayload(payload.chrome_mode),
  };
}

async function getNativeDesktopChrome(): Promise<NativeDesktopChrome> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return browserDesktopChrome;
  const payload = await tauri.core.invoke('desktop_chrome');
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
  callback: (data: unknown) => void,
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

function nativeDragDropPayload(payload: unknown): NativeDragDropPayload | null {
  if (!isDtoRecord(payload)) return null;
  const record = payload;
  const type = record.type;
  if (type !== 'enter' && type !== 'over' && type !== 'drop' && type !== 'leave') return null;
  const rawPosition = record.position;
  const positionRecord = isDtoRecord(rawPosition) ? rawPosition : null;
  const x = positionRecord?.x;
  const y = positionRecord?.y;
  const position =
    typeof x === 'number' && typeof y === 'number' && Number.isFinite(x) && Number.isFinite(y) ? { x, y } : null;
  return {
    type,
    eligible: record.eligible === true,
    token: typeof record.token === 'string' && record.token ? record.token : null,
    position,
    error: typeof record.error === 'string' && record.error ? record.error : null,
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
  const value = await tauri.core.invoke('app_version');
  if (typeof value !== 'string' || !value.trim()) throw new Error('Native app version response was invalid.');
  return value;
}

export interface NativeApiTransportBootstrap {
  base_url: string;
  capability: string;
}

export async function getNativeApiTransportBootstrap(): Promise<NativeApiTransportBootstrap | null> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return null;
  const value = await tauri.core.invoke('api_transport_bootstrap');
  const record = dtoRecord(value, 'Native API transport bootstrap');
  const baseUrl = dtoString(record.base_url, 'Native API transport base URL');
  const capability = dtoString(record.capability, 'Native API transport capability');
  if (!baseUrl || !capability) {
    throw new Error('Native API transport bootstrap was invalid.');
  }
  return { base_url: baseUrl, capability };
}

export async function signInWithMicrosoft(): Promise<NativeMicrosoftSignInResult | undefined> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return undefined;
  const value = await tauri.core.invoke('microsoft_sign_in');
  const record = dtoRecord(value, 'Native Microsoft sign-in');
  if (record.status !== 'authenticated' && record.status !== 'cancelled') {
    throw new Error('Native Microsoft sign-in response was invalid.');
  }
  for (const key of ['login_id', 'profile_name'] as const) {
    if (record[key] !== undefined && record[key] !== null && typeof record[key] !== 'string') {
      throw new Error('Native Microsoft sign-in response was invalid.');
    }
  }
  if (
    record.owns_minecraft_java !== undefined &&
    record.owns_minecraft_java !== null &&
    typeof record.owns_minecraft_java !== 'boolean'
  ) {
    throw new Error('Native Microsoft sign-in response was invalid.');
  }
  const loginId =
    record.login_id == null ? (record.login_id === null ? null : undefined) : dtoString(record.login_id, 'Login id');
  const profileName =
    record.profile_name == null
      ? record.profile_name === null
        ? null
        : undefined
      : dtoString(record.profile_name, 'Profile name');
  const ownsMinecraftJava =
    record.owns_minecraft_java == null
      ? record.owns_minecraft_java === null
        ? null
        : undefined
      : record.owns_minecraft_java;
  return {
    status: record.status,
    login_id: loginId,
    profile_name: profileName,
    owns_minecraft_java: ownsMinecraftJava,
  };
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
  const record = dtoRecord(payload, 'Native skin picker');
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
  const payload = await tauri.core.invoke('pick_skin_file');
  return payload === null ? null : nativeSkinFileFromPayload(payload);
}

export async function consumeNativeSkinDrop(token: string): Promise<File | undefined> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return undefined;

  const payload = await tauri.core.invoke('consume_skin_drop', { token });
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
  const value = await tauri.core.invoke('window_toggle_maximize');
  if (typeof value !== 'boolean') throw new Error('Native maximize response was invalid.');
  return value;
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
  const value = await tauri.core.invoke('window_is_maximized');
  if (typeof value !== 'boolean') throw new Error('Native maximize state was invalid.');
  return value;
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
