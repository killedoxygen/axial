import { signal, computed } from '@preact/signals';
import type { LaunchSession, InstanceLaunchDraft, LaunchNotice } from './types-launch';
import type { Version } from './types-version';
import type { Instance } from './types-instance';
import type { Config, SystemInfo } from './types-settings';
import type { ToastItem } from './types-ui';
import type { UpdateInfo } from './types-update';
import type { FeatureFlagViewModel, FeatureFlagsLoadState } from './types-flags';

export const instances = signal<Instance[]>([]);
export const versions = signal<Version[]>([]);
export const config = signal<Config | null>(null);
export const systemInfo = signal<SystemInfo | null>(null);
export const devMode = signal(false);
export const featureFlags = signal<FeatureFlagViewModel[] | null>(null);
export const featureFlagsLoadState = signal<FeatureFlagsLoadState>({ status: 'idle', error: null });
export const lastInstanceId = signal<string | null>(null);

export const selectedInstanceId = signal<string | null>(null);

export const selectedInstance = computed<Instance | null>(() => {
  const id = selectedInstanceId.value;
  if (!id) return null;
  return instances.value.find((i) => i.id === id) ?? null;
});

export function versionById(id: string | undefined): Version | undefined {
  if (!id) return undefined;
  return versions.value.find((v) => v.id === id);
}

export type LaunchState =
  | { status: 'idle' }
  | {
      status: 'preparing';
      instanceId: string;
      pct: number;
      label: string;
      stage?: string;
      determinate?: boolean;
    };

export const launchState = signal<LaunchState>({ status: 'idle' });
export const launchSessions = signal<Record<string, LaunchSession>>({});
export const instanceLaunchDrafts = signal<Record<string, InstanceLaunchDraft>>({});
export const launchNotices = signal<Record<string, LaunchNotice>>({});

export type LogSeverity = 'error' | 'system' | 'info';
export const logLines = signal(0);
export const collapsedLogSeverity = signal<LogSeverity | null>(null);
export const collapsedGroups = signal<Record<string, boolean>>({});
export const bootstrapState = signal<'loading' | 'ready' | 'error'>('loading');
export const bootstrapError = signal<string | null>(null);
export const appVersion = signal('1.1.0');
export const toasts = signal<ToastItem[]>([]);
export const updateInfo = signal<UpdateInfo | null>(null);
export const updateCheckState = signal<'idle' | 'checking' | 'ready' | 'error'>('idle');
