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

export async function getNativeAppVersion(): Promise<string | null> {
  const tauri = getTauriBinding();
  if (!tauri?.core) return null;
  return tauri.core.invoke<string>('app_version');
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

export async function openExternalURL(url: string): Promise<void> {
  const tauri = getTauriBinding();
  if (tauri?.opener) {
    await tauri.opener.openUrl(url);
    return;
  }

  window.open(url, '_blank', 'noopener,noreferrer');
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
