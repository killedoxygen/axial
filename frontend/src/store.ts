import { signal, computed } from '@preact/signals';
import type {
  Instance, Version, Config, SystemInfo,
  RunningSession, InstallItem, Catalog, Page, ToastItem, UpdateInfo,
} from './types';

// ── Core data ──

export const instances = signal<Instance[]>([]);
export const versions = signal<Version[]>([]);
export const config = signal<Config | null>(null);
export const systemInfo = signal<SystemInfo | null>(null);
export const devMode = signal(false);
export const catalog = signal<Catalog | null>(null);
export const lastInstanceId = signal<string | null>(null);

// ── Selection ──

export const selectedInstanceId = signal<string | null>(null);

export const selectedInstance = computed<Instance | null>(() => {
  const id = selectedInstanceId.value;
  if (!id) return null;
  return instances.value.find(i => i.id === id) ?? null;
});

export const selectedVersion = computed<Version | null>(() => {
  const inst = selectedInstance.value;
  if (!inst) return null;
  return versions.value.find(v => v.id === inst.version_id) ?? null;
});

// ── Install state machine ──

export type InstallState =
  | { status: 'idle' }
  | { status: 'active'; versionId: string; pct: number; label: string };

export const installState = signal<InstallState>({ status: 'idle' });
export const installQueue = signal<InstallItem[]>([]);
export const installEventSource = signal<{ close(): void } | null>(null);

// ── Launch state machine ──

export type LaunchState =
  | { status: 'idle' }
  | { status: 'preparing'; instanceId: string };

export const launchState = signal<LaunchState>({ status: 'idle' });
export const runningSessions = signal<Record<string, RunningSession>>({});

// ── UI state ──

export const currentPage = signal<Page>('launcher');
export const searchQuery = signal('');
export const sidebarFilter = signal('all');
export const logLines = signal(0);
export const collapsedGroups = signal<Record<string, boolean>>({});
export const bootstrapState = signal<'loading' | 'ready' | 'error'>('loading');
export const bootstrapError = signal<string | null>(null);
export const appVersion = signal('1.1.0');
export const toasts = signal<ToastItem[]>([]);
export const updateInfo = signal<UpdateInfo | null>(null);
export const updateCheckState = signal<'idle' | 'checking' | 'ready' | 'error'>('idle');

// ── Derived state ──

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

  if (filter === 'release') {
    list = list.filter(inst => {
      const v = vm.get(inst.version_id);
      return v?.type === 'release' && !v?.inherits_from;
    });
  } else if (filter === 'snapshot') {
    list = list.filter(inst => {
      const v = vm.get(inst.version_id);
      return v?.type === 'snapshot' && !v?.inherits_from;
    });
  } else if (filter === 'modded') {
    list = list.filter(inst => {
      const v = vm.get(inst.version_id);
      return !!v?.inherits_from;
    });
  }

  if (search) {
    const q = search.toLowerCase();
    list = list.filter(inst =>
      inst.name.toLowerCase().includes(q) ||
      inst.version_id.toLowerCase().includes(q)
    );
  }

  return list;
});
