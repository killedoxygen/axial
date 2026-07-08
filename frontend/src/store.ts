import { signal, computed } from '@preact/signals';
import type { RunningSession, InstanceLaunchDraft, LaunchNotice } from './types-launch';
import type { Version, Catalog } from './types-version';
import type { Instance } from './types-instance';
import type { Config, SystemInfo } from './types-settings';
import type { Page, ToastItem } from './types-ui';
import type { UpdateInfo } from './types-update';
import type { FeatureFlagViewModel, FeatureFlagsLoadState } from './types-flags';

export const instances = signal<Instance[]>([]);
export const versions = signal<Version[]>([]);
export const config = signal<Config | null>(null);
export const systemInfo = signal<SystemInfo | null>(null);
export const devMode = signal(false);
export const featureFlags = signal<FeatureFlagViewModel[] | null>(null);
export const featureFlagsLoadState = signal<FeatureFlagsLoadState>({ status: 'idle', error: null });
export const catalog = signal<Catalog | null>(null);
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

export const selectedVersion = computed<Version | null>(() => {
  return versionById(selectedInstance.value?.version_id) ?? null;
});

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
export const runningSessions = signal<Record<string, RunningSession>>({});
export const instanceLaunchDrafts = signal<Record<string, InstanceLaunchDraft>>({});
export const launchNotices = signal<Record<string, LaunchNotice>>({});

export const currentPage = signal<Page>('launcher');
export const searchQuery = signal('');
export const sidebarFilter = signal('all');
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

export const versionMap = computed<Map<string, Version>>(() => {
  const map = new Map<string, Version>();
  for (const v of versions.value) map.set(v.id, v);
  return map;
});

export const filteredInstances = computed<Instance[]>(() => {
  let list = instances.value;
  const vm = versionMap.value;
  const filter = sidebarFilter.value;
  const search = searchQuery.value;

  const isRelease = (version: Version | undefined) =>
    version?.lifecycle?.channel === 'stable' && version.lifecycle.labels.includes('release');
  const isSnapshot = (version: Version | undefined) =>
    !!version?.lifecycle &&
    !version.lifecycle.labels.includes('old_beta') &&
    !version.lifecycle.labels.includes('old_alpha') &&
    (version.lifecycle.channel === 'preview' || version.lifecycle.channel === 'experimental');

  if (filter === 'release') {
    list = list.filter((inst) => {
      const v = vm.get(inst.version_id);
      return isRelease(v) && !v?.inherits_from;
    });
  } else if (filter === 'snapshot') {
    list = list.filter((inst) => {
      const v = vm.get(inst.version_id);
      return isSnapshot(v) && !v?.inherits_from;
    });
  } else if (filter === 'modded') {
    list = list.filter((inst) => {
      const v = vm.get(inst.version_id);
      return !!v?.inherits_from;
    });
  }

  if (search) {
    const q = search.toLowerCase();
    list = list.filter((inst) => inst.name.toLowerCase().includes(q) || inst.version_id.toLowerCase().includes(q));
  }

  return list;
});
