import { EventsOn } from './wailsjs/runtime/runtime';

interface WailsAppBinding {
  Version(): Promise<string>;
  BrowseDirectory(defaultPath: string): Promise<string>;
  OpenExternalURL(url: string): Promise<void>;
  ShowNotice(title: string, message: string): Promise<void>;
  StartInstallEvents(installId: string): Promise<void>;
  StartLoaderInstallEvents(installId: string): Promise<void>;
  StartLaunchEvents(sessionId: string): Promise<void>;
}

declare global {
  interface Window {
    go?: {
      main?: {
        App?: WailsAppBinding;
      };
    };
  }
}

function getBinding(): WailsAppBinding | null {
  return window.go?.main?.App ?? null;
}

export function isWailsRuntime(): boolean {
  return getBinding() !== null;
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

export function onNativeEvent(eventName: string, callback: (data: any) => void): { close(): void } | null {
  if (!isWailsRuntime()) return null;
  const unsubscribe = EventsOn(eventName, (...data: any[]) => callback(data[0]));
  return { close: unsubscribe };
}

export async function getNativeAppVersion(): Promise<string | null> {
  const binding = getBinding();
  if (!binding) return null;
  return binding.Version();
}

export async function browseDirectory(defaultPath = ''): Promise<string | null> {
  const binding = getBinding();
  if (!binding) return null;
  return binding.BrowseDirectory(defaultPath);
}

export async function openExternalURL(url: string): Promise<void> {
  const binding = getBinding();
  if (!binding) {
    window.open(url, '_blank', 'noopener,noreferrer');
    return;
  }
  await binding.OpenExternalURL(url);
}

export async function showNativeNotice(title: string, message: string): Promise<boolean> {
  const binding = getBinding();
  if (!binding) return false;
  await binding.ShowNotice(title, message);
  return true;
}

export async function startNativeInstallEvents(installId: string): Promise<boolean> {
  const binding = getBinding();
  if (!binding) return false;
  await binding.StartInstallEvents(installId);
  return true;
}

export async function startNativeLoaderInstallEvents(installId: string): Promise<boolean> {
  const binding = getBinding();
  if (!binding) return false;
  await binding.StartLoaderInstallEvents(installId);
  return true;
}

export async function startNativeLaunchEvents(sessionId: string): Promise<boolean> {
  const binding = getBinding();
  if (!binding) return false;
  await binding.StartLaunchEvents(sessionId);
  return true;
}
