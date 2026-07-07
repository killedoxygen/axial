import type { Config, SystemInfo } from '../types-settings';
import type { EnrichedInstance } from '../types-instance';
import type { InstallQueueStateResponse } from '../types-install';
import type { FeatureFlagViewModel, FlagsResponse } from '../types-flags';
import type { Version } from '../types-version';

type Handler = (body?: unknown, path?: string) => unknown | Promise<unknown>;

interface StatusResponse {
  status: string;
  warnings: string[];
  library_dir: string;
  library_mode: string;
  setup_required: boolean;
  app_name: string;
  version: string;
  dev_mode: boolean;
}

interface VersionsResponse {
  versions: Version[];
  scan_state: ScanState;
}

interface InstancesResponse {
  instances: EnrichedInstance[];
  last_instance_id: string | null;
  scan_state: ScanState;
}

interface ScanState {
  state_id: string;
  label: string;
  degraded: boolean;
  detail?: string;
}

interface ApiError extends Error {
  name: 'ApiError';
  status: number;
  statusText: string;
  payload?: unknown;
}

interface FlagDefinition {
  key: string;
  title: string;
  description: string;
  stage: 'experimental' | 'beta';
  dev_only: boolean;
  default_enabled: boolean;
}

console.info('Croopor mock API active — data is fake, no backend running');

const missingHandlers = new Set<string>();
const flagOverrides = new Map<string, boolean>();

let configFixture: Config = {
  username: 'MockPlayer',
  launch_auth_mode: 'offline',
  max_memory_mb: 4096,
  min_memory_mb: 1024,
  java_path_override: '',
  window_width: 1280,
  window_height: 720,
  jvm_preset: '',
  performance_mode: 'managed',
  guardian_mode: 'managed',
  theme: 'obsidian',
  custom_hue: 140,
  custom_vibrancy: 100,
  lightness: 0,
  onboarding_done: true,
  telemetry_enabled: false,
  discord_rpc_enabled: true,
  discord_rpc_onboarding_seen: true,
  library_dir: '/mock/Croopor Library',
  library_mode: 'managed',
  music_enabled: false,
  music_volume: 35,
  music_track: 0,
};

const scanState: ScanState = {
  state_id: 'ready',
  label: 'Versions ready',
  degraded: false,
};

const flagRegistry: FlagDefinition[] = [
  {
    key: 'dev.state-inspector',
    title: 'State inspector',
    description: 'Show the live state inspector tab in the Dev Lab.',
    stage: 'experimental',
    dev_only: true,
    default_enabled: false,
  },
];

const handlers: Record<string, Handler> = {
  'GET /config': () => configFixture,
  'PUT /config': (body) => {
    if (isRecord(body)) {
      configFixture = { ...configFixture, ...body };
    }
    return configFixture;
  },
  'GET /status': (): StatusResponse => ({
    status: 'ok',
    warnings: [],
    library_dir: configFixture.library_dir ?? '',
    library_mode: configFixture.library_mode ?? 'managed',
    setup_required: false,
    app_name: 'Croopor',
    version: 'mock-dev',
    dev_mode: true,
  }),
  'GET /system': (): SystemInfo => ({
    total_memory_mb: 16384,
    recommended_min_mb: 4096,
    recommended_max_mb: 8192,
    max_allocatable_gb: 16,
  }),
  'GET /music/status': () => ({
    tracks: [],
    count: 0,
  }),
  'GET /versions': (): VersionsResponse => ({
    versions: versionFixtures,
    scan_state: scanState,
  }),
  'GET /instances': (): InstancesResponse => ({
    instances: instanceFixtures,
    last_instance_id: 'mock-fabric-lab',
    scan_state: scanState,
  }),
  'GET /install/queue': (): InstallQueueStateResponse => ({
    active: null,
    items: [],
    view_model: {
      state_id: 'empty',
      status_label: 'Idle',
      title: 'Install queue',
      summary: 'No installs are queued.',
      queued_count: 0,
      queued_count_label: '0 queued',
      queued_item_label: 'No queued installs',
      next_label: null,
      active_queued_count_label: null,
      section_title: 'Queue',
      empty_title: 'No installs queued',
      empty_summary: 'Install requests will appear here.',
    },
    notice: null,
    started_install: null,
  }),
  'GET /flags': () => flagsResponse(),
  'PUT /flags/{key}': (body, path) => updateFlag(path?.slice('/flags/'.length) ?? '', body),
  'POST /telemetry/frontend-error': () => null,
};

const versionFixtures: Version[] = [
  vanillaVersion('1.21.6', '2025-06-17T12:00:00Z', true),
  vanillaVersion('1.21.5', '2025-03-25T12:00:00Z', true),
  vanillaVersion('1.20.1', '2023-06-12T12:00:00Z', true),
  {
    ...vanillaVersion('fabric-loader-0.16.14-1.21.5', '2025-03-26T12:00:00Z', true),
    raw_kind: 'fabric',
    inherits_from: '1.21.5',
    minecraft_meta: minecraftMeta('1.21.5'),
    loader: {
      component_id: 'net.fabricmc.fabric-loader',
      component_name: 'Fabric Loader',
      build_id: 'fabric-loader-0.16.14-1.21.5',
      loader_version: '0.16.14',
      build_meta: {
        terms: ['recommended'],
        evidence: [{ term: 'recommended', source: 'explicit_version_label' }],
        selection: {
          default_rank: 100,
          reason: 'recommended',
          source: 'explicit_version_label',
        },
        display_tags: ['stable'],
      },
    },
  },
];

const instanceFixtures: EnrichedInstance[] = [
  instanceFixture({
    id: 'mock-survival',
    name: 'Survival Ridge',
    version_id: '1.21.6',
    created_at: '2026-07-01T10:00:00.000Z',
    last_played_at: '2026-07-06T20:15:00.000Z',
    art_seed: 12814,
    accent: 'emerald',
    saves_count: 1,
    resource_count: 2,
  }),
  instanceFixture({
    id: 'mock-fabric-lab',
    name: 'Fabric Lab',
    version_id: 'fabric-loader-0.16.14-1.21.5',
    created_at: '2026-06-20T14:30:00.000Z',
    last_played_at: '2026-07-05T18:05:00.000Z',
    art_seed: 84291,
    accent: 'amethyst',
    mods_count: 12,
    resource_count: 4,
    shader_count: 1,
    version_display: {
      loader_key: 'fabric',
      loader_label: 'Fabric',
      minecraft_label: '1.21.5',
      loader_version_label: '0.16.14',
      loader_detail_label: 'Fabric - 0.16.14',
      summary_label: 'Fabric - 1.21.5',
      supports_mods: true,
    },
  }),
  instanceFixture({
    id: 'mock-classic',
    name: 'Archive 1.20',
    version_id: '1.20.1',
    created_at: '2026-05-11T09:00:00.000Z',
    art_seed: 3975,
    accent: 'gold',
  }),
];

export async function mockApi<T>(method: string, path: string, body?: unknown): Promise<T> {
  const normalizedMethod = method.toUpperCase();
  const normalizedPath = normalizePath(path);
  const key = handlerKey(normalizedMethod, normalizedPath);
  const handler = handlers[key];
  if (!handler) throw missingHandlerError(key);
  return (await handler(body, normalizedPath)) as T;
}

function handlerKey(method: string, path: string): string {
  if (method === 'PUT' && path.startsWith('/flags/')) return 'PUT /flags/{key}';
  return `${method} ${path}`;
}

function updateFlag(encodedKey: string, body: unknown): FlagsResponse {
  const key = decodeURIComponent(encodedKey);
  const flag = flagRegistry.find((entry) => entry.key === key);
  if (!flag) throw apiError(404, 'Not Found', { error: 'unknown feature flag' });
  if (isRecord(body) && typeof body.enabled === 'boolean') {
    flagOverrides.set(flag.key, body.enabled);
  } else {
    flagOverrides.delete(flag.key);
  }
  return flagsResponse();
}

function flagsResponse(): FlagsResponse {
  return {
    flags: flagRegistry.map((flag): FeatureFlagViewModel => {
      const override = flagOverrides.get(flag.key);
      return {
        ...flag,
        enabled: override ?? flag.default_enabled,
        source: override === undefined ? 'default' : 'override',
      };
    }),
  };
}

function missingHandlerError(key: string): ApiError {
  if (!missingHandlers.has(key)) {
    missingHandlers.add(key);
    console.warn(`mock API: no handler for ${key}`);
  }
  return apiError(501, 'Not Implemented', { error: 'not mocked' });
}

function apiError(status: number, statusText: string, payload: unknown): ApiError {
  const error = new Error(errorMessage(status, statusText, payload)) as ApiError;
  error.name = 'ApiError';
  error.status = status;
  error.statusText = statusText;
  error.payload = payload;
  return error;
}

function errorMessage(status: number, statusText: string, payload: unknown): string {
  if (isRecord(payload) && typeof payload.error === 'string' && payload.error.trim()) {
    return payload.error.trim();
  }
  return `Request failed with HTTP ${status}${statusText ? ` ${statusText}` : ''}`;
}

function normalizePath(path: string): string {
  const withoutQuery = path.split(/[?#]/, 1)[0] || '/';
  const apiPrefix = '/api/v1';
  const unprefixed = withoutQuery.startsWith(apiPrefix) ? withoutQuery.slice(apiPrefix.length) || '/' : withoutQuery;
  return unprefixed.startsWith('/') ? unprefixed : `/${unprefixed}`;
}

function vanillaVersion(id: string, releaseTime: string, installed: boolean): Version {
  return {
    subject_kind: 'installed_version',
    id,
    raw_kind: 'release',
    release_time: releaseTime,
    minecraft_meta: minecraftMeta(id),
    lifecycle: {
      channel: 'stable',
      labels: ['release'],
      default_rank: 100,
      badge_text: 'Release',
      provider_terms: [],
    },
    inherits_from: '',
    launchable: installed,
    installed,
    status: installed ? 'installed' : 'missing',
    status_detail: installed ? '' : 'Version files are not installed.',
    needs_install: installed ? '' : 'client',
    java_component: 'java-runtime-delta',
    java_major: 21,
    manifest_url: '',
    loader: null,
  };
}

function minecraftMeta(id: string): Version['minecraft_meta'] {
  return {
    family: id,
    base_id: id,
    effective_version: id,
    variant_of: '',
    variant_kind: '',
    display_name: id,
    display_hint: '',
  };
}

function instanceFixture(
  input: Partial<EnrichedInstance> & Pick<EnrichedInstance, 'id' | 'name' | 'version_id' | 'created_at'>,
): EnrichedInstance {
  return {
    id: input.id,
    name: input.name,
    version_id: input.version_id,
    created_at: input.created_at,
    last_played_at: input.last_played_at,
    art_seed: input.art_seed ?? 1,
    max_memory_mb: input.max_memory_mb,
    min_memory_mb: input.min_memory_mb,
    java_path: '',
    window_width: 0,
    window_height: 0,
    jvm_preset: '',
    performance_mode: input.performance_mode ?? '',
    extra_jvm_args: '',
    icon: input.icon ?? '',
    accent: input.accent ?? '',
    version_display:
      input.version_display ??
      ({
        loader_key: 'vanilla',
        loader_label: 'Vanilla',
        minecraft_label: input.version_id,
        loader_version_label: '',
        loader_detail_label: 'Vanilla',
        summary_label: `Vanilla - ${input.version_id}`,
        supports_mods: false,
      } satisfies EnrichedInstance['version_display']),
    launchable: input.launchable ?? true,
    launch_action: {
      state_id: 'launch_ready',
      label: 'Launch',
      tone: 'ok',
      launchable: true,
      primary_action: 'launch',
    },
    status_detail: '',
    needs_install: '',
    java_major: 21,
    saves_count: input.saves_count ?? 0,
    mods_count: input.mods_count ?? 0,
    resource_count: input.resource_count ?? 0,
    shader_count: input.shader_count ?? 0,
  };
}

function isRecord(value: unknown): value is Record<string, any> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}
