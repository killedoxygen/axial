import type { Config, SystemInfo } from '../types-settings';
import type { EnrichedInstance } from '../types-instance';
import type { InstallQueueStateResponse } from '../types-install';
import type { FeatureFlagViewModel, FlagsResponse } from '../types-flags';
import type { UpdateFlowState, UpdateInfo } from '../types-update';
import type { Version } from '../types-version';

type Handler = (body?: unknown, path?: string, request?: MockRequest) => unknown | Promise<unknown>;

interface MockRequest {
  path: string;
  searchParams: URLSearchParams;
}

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

interface CreateOption {
  id: string;
  label: string;
  enabled: boolean;
  disabled_reason?: string | null;
}

interface CreateVersionRow {
  source_id: string;
  selection_id: string;
  minecraft_version_id: string;
  display_name: string;
  hint?: string | null;
  channel: string;
  tags: CreateVersionTag[];
  download_state: string;
  create_enabled: boolean;
  disabled_reason?: string | null;
}

interface CreateVersionTag {
  id: string;
  label: string;
}

interface CreatePresetOption {
  id: string;
  label: string;
  detail: string;
  default: boolean;
  disabled_reason?: string | null;
}

interface CreateOptimizeOption {
  id: string;
  label: string;
  detail: string;
  default_enabled: boolean;
}

interface CreateInstanceViewResponse {
  sources: CreateOption[];
  channels: CreateOption[];
  versions: CreateVersionRow[];
  preset_options: CreatePresetOption[];
  optimize_option: CreateOptimizeOption;
  defaults: {
    source_id: string;
    channel_id: string;
    jvm_preset_id: string;
    max_memory_mb?: number | null;
    window_width?: number | null;
    window_height?: number | null;
  };
  notices: [];
}

interface CreateLoaderBuildsViewResponse {
  source_id: string;
  minecraft_version_id: string;
  auto: {
    selection_id: string;
    label: string;
    detail: string;
  };
  builds: Array<{
    selection_id: string;
    build_id: string;
    label: string;
    channel_id: string;
    channel_label: string;
    recommended: boolean;
    installed: boolean;
    enabled: boolean;
    disabled_reason?: string | null;
  }>;
}

interface CreateInstanceResponse extends EnrichedInstance {
  result: {
    command: 'CreateInstance';
    operation_id: null;
    status: 'succeeded';
    safety: null;
    carriers: Record<string, never>;
    payload: {
      instance_id: string;
      queue_id: null;
      install_id: null;
      operation_id: null;
    };
    view_model: null;
  };
  view_model: {
    state_id: 'created';
    tone: 'success';
    title: 'Instance created';
    summary: string;
    detail: null;
  };
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

console.info('Axial mock API active — data is fake, no backend running');

const missingHandlers = new Set<string>();
const flagOverrides = new Map<string, boolean>();

const FABRIC_COMPONENT_ID = 'net.fabricmc.fabric-loader';
const MOCK_FABRIC_MC_VERSION = '1.21.5';
const MOCK_FABRIC_BUILD_ID = 'fabric-loader-0.16.14-1.21.5';
const MOCK_FABRIC_LOADER_VERSION = '0.16.14';

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
  library_dir: '/mock/Axial Library',
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
    app_name: 'Axial',
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
    last_instance_id: lastInstanceId,
    scan_state: scanState,
  }),
  'GET /instances/create-view': (_body, _path, request): CreateInstanceViewResponse => createInstanceView(request),
  'GET /instances/create-view/loader-builds': (_body, _path, request): CreateLoaderBuildsViewResponse =>
    createLoaderBuildsView(request),
  'POST /instances': (body): CreateInstanceResponse => createInstance(body),
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
  'GET /instances/{id}': (_body, path) => findInstance(instanceIdFromPath(path)),
  'PUT /instances/{id}': (body, path) => updateInstance(instanceIdFromPath(path), body),
  'GET /java': () => ({
    runtimes: [
      { path: '/mock/java/21/bin/java', component: 'java-runtime-delta-21', source: 'managed' },
      { path: '/mock/java/17/bin/java', component: 'java-runtime-gamma-17', source: 'system' },
    ],
  }),
  'GET /performance/health': () => ({ health: null }),
  'GET /update': (): UpdateInfo & { checked_at: string } => mockUpdateInfo(),
  'GET /update/flow': () => mockUpdateFlow(),
  'POST /update/download': () => startMockUpdateDownload(),
  'POST /update/apply': () => applyMockUpdate(),
};

const MOCK_UPDATE_VERSION = '9.9.9';
const MOCK_UPDATE_TOTAL_BYTES = 48 * 1024 * 1024;
const MOCK_UPDATE_DOWNLOAD_MS = 6500;

let mockUpdateDownloadStartedAt: number | null = null;
let mockUpdateApplied = false;

function mockUpdateInfo(): UpdateInfo & { checked_at: string } {
  return {
    current_version: 'mock-dev',
    latest_version: MOCK_UPDATE_VERSION,
    available: true,
    platform: 'mock',
    arch: 'mock',
    kind: 'release-asset',
    install_mode: 'in-app',
    notes_url: `https://github.com/mateoltd/axial/releases/tag/v${MOCK_UPDATE_VERSION}`,
    action_url: `https://github.com/mateoltd/axial/releases/download/v${MOCK_UPDATE_VERSION}/axial-mock-${MOCK_UPDATE_VERSION}.tar.gz`,
    checksum_url: null,
    action_label: 'Download update',
    checked_at: new Date().toISOString(),
  };
}

function mockUpdateFlow(): UpdateFlowState {
  if (mockUpdateApplied) {
    return {
      phase: 'restart-pending',
      version: MOCK_UPDATE_VERSION,
      received_bytes: MOCK_UPDATE_TOTAL_BYTES,
      total_bytes: MOCK_UPDATE_TOTAL_BYTES,
      percent: 100,
      message: '',
    };
  }
  if (mockUpdateDownloadStartedAt === null) {
    return { phase: 'idle', version: '', received_bytes: 0, total_bytes: null, percent: null, message: '' };
  }
  const elapsed = Date.now() - mockUpdateDownloadStartedAt;
  const fraction = Math.min(1, elapsed / MOCK_UPDATE_DOWNLOAD_MS);
  if (fraction >= 1) {
    return {
      phase: 'ready',
      version: MOCK_UPDATE_VERSION,
      received_bytes: MOCK_UPDATE_TOTAL_BYTES,
      total_bytes: MOCK_UPDATE_TOTAL_BYTES,
      percent: 100,
      message: '',
    };
  }
  return {
    phase: 'downloading',
    version: MOCK_UPDATE_VERSION,
    received_bytes: Math.round(MOCK_UPDATE_TOTAL_BYTES * fraction),
    total_bytes: MOCK_UPDATE_TOTAL_BYTES,
    percent: Math.round(fraction * 100),
    message: '',
  };
}

function startMockUpdateDownload(): UpdateFlowState {
  if (mockUpdateApplied) throw apiError(409, 'Conflict', { error: 'an update is already applied; restart to finish' });
  mockUpdateDownloadStartedAt = Date.now();
  return mockUpdateFlow();
}

function applyMockUpdate(): UpdateFlowState {
  if (mockUpdateFlow().phase !== 'ready') {
    throw apiError(409, 'Conflict', { error: 'no staged update is ready to apply' });
  }
  mockUpdateApplied = true;
  return mockUpdateFlow();
}

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

let lastInstanceId: string | null = 'mock-fabric-lab';

export async function mockApi<T>(method: string, path: string, body?: unknown): Promise<T> {
  const normalizedMethod = method.toUpperCase();
  const request = normalizeRequest(path);
  const normalizedPath = request.path;
  const key = handlerKey(normalizedMethod, normalizedPath);
  const handler = handlers[key];
  if (!handler) throw missingHandlerError(key);
  return cloneJsonResponse(await handler(body, normalizedPath, request)) as T;
}

function handlerKey(method: string, path: string): string {
  if (method === 'PUT' && path.startsWith('/flags/')) return 'PUT /flags/{key}';
  if (/^\/instances\/[^/]+$/.test(path) && path !== '/instances/create-view') return `${method} /instances/{id}`;
  return `${method} ${path}`;
}

function instanceIdFromPath(path: string | undefined): string {
  return decodeURIComponent((path ?? '').slice('/instances/'.length));
}

function findInstance(id: string): EnrichedInstance {
  const instance = instanceFixtures.find((fixture) => fixture.id === id);
  if (!instance) throw apiError(404, 'Not Found', { error: 'unknown instance' });
  return instance;
}

function updateInstance(id: string, body: unknown): EnrichedInstance {
  const instance = findInstance(id);
  if (isRecord(body)) {
    if (typeof body.name === 'string' && body.name.trim()) instance.name = body.name.trim();
    for (const key of ['max_memory_mb', 'min_memory_mb', 'window_width', 'window_height', 'art_seed'] as const) {
      const value = finiteNumber(body[key]);
      if (value !== undefined) instance[key] = Math.max(0, Math.round(value));
    }
    for (const key of ['jvm_preset', 'java_path', 'extra_jvm_args'] as const) {
      if (typeof body[key] === 'string') instance[key] = body[key];
    }
    const mode = body.performance_mode;
    if (mode === '' || mode === 'managed' || mode === 'vanilla' || mode === 'custom') {
      instance.performance_mode = mode;
    }
  }
  return instance;
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

function createInstanceView(request: MockRequest | undefined): CreateInstanceViewResponse {
  const requestedSource = request?.searchParams.get('source')?.trim() || 'vanilla';
  const sourceId = createSourceOptions().some((option) => option.id === requestedSource) ? requestedSource : 'vanilla';
  return {
    sources: createSourceOptions(),
    channels: [
      { id: 'release', label: 'Release', enabled: true },
      { id: 'snapshot', label: 'Snapshot', enabled: true },
      { id: 'legacy', label: 'Legacy', enabled: true },
    ],
    versions: createVersionRows().filter((row) => row.source_id === sourceId),
    preset_options: createPresetOptions(),
    optimize_option: {
      id: 'auto_optimize',
      label: 'Auto-optimize',
      detail: "Axial tunes this instance's performance while you play.",
      default_enabled: true,
    },
    defaults: {
      source_id: 'vanilla',
      channel_id: 'release',
      jvm_preset_id: '',
      max_memory_mb: configFixture.max_memory_mb,
      window_width: configFixture.window_width,
      window_height: configFixture.window_height,
    },
    notices: [],
  };
}

function createLoaderBuildsView(request: MockRequest | undefined): CreateLoaderBuildsViewResponse {
  const sourceId = request?.searchParams.get('source')?.trim() ?? '';
  const minecraftVersion = request?.searchParams.get('minecraft_version')?.trim() ?? '';
  if (sourceId !== FABRIC_COMPONENT_ID) {
    throw apiError(404, 'Not Found', { error: 'unknown loader component' });
  }
  if (minecraftVersion !== MOCK_FABRIC_MC_VERSION) {
    throw apiError(404, 'Not Found', { error: 'no mock loader builds for this Minecraft version' });
  }
  return {
    source_id: FABRIC_COMPONENT_ID,
    minecraft_version_id: MOCK_FABRIC_MC_VERSION,
    auto: {
      selection_id: `loader_version|${FABRIC_COMPONENT_ID}|${MOCK_FABRIC_MC_VERSION}`,
      label: 'Automatic',
      detail: 'Axial picks the newest stable Fabric build.',
    },
    builds: [
      {
        selection_id: `loader_build|${FABRIC_COMPONENT_ID}|${MOCK_FABRIC_BUILD_ID}`,
        build_id: MOCK_FABRIC_BUILD_ID,
        label: MOCK_FABRIC_LOADER_VERSION,
        channel_id: 'stable',
        channel_label: 'Stable',
        recommended: true,
        installed: true,
        enabled: true,
        disabled_reason: null,
      },
    ],
  };
}

function createInstance(body: unknown): CreateInstanceResponse {
  if (!isRecord(body)) {
    throw apiError(400, 'Bad Request', { error: 'request body is required' });
  }
  const name = typeof body.name === 'string' ? body.name.trim() : '';
  const selectionId = typeof body.selection_id === 'string' ? body.selection_id.trim() : '';
  if (!name) throw apiError(400, 'Bad Request', { error: 'name is required' });
  if (!selectionId) throw apiError(400, 'Bad Request', { error: 'selection_id is required' });

  const selection = createSelection(selectionId);
  const now = new Date().toISOString();
  const created = instanceFixture({
    id: uniqueInstanceId(name),
    name,
    version_id: selection.versionId,
    created_at: now,
    art_seed: finiteNumber(body.art_seed) ?? nextArtSeed(name),
    max_memory_mb: finiteNumber(body.max_memory_mb),
    min_memory_mb: finiteNumber(body.min_memory_mb),
    window_width: finiteNumber(body.window_width),
    window_height: finiteNumber(body.window_height),
    jvm_preset: typeof body.jvm_preset_id === 'string' ? body.jvm_preset_id : '',
    performance_mode: body.auto_optimize === false ? '' : 'managed',
    icon: typeof body.icon === 'string' ? body.icon : '',
    accent: typeof body.accent === 'string' ? body.accent : '',
    version_display: selection.versionDisplay,
    mods_count: selection.supportsMods ? 0 : undefined,
  });
  instanceFixtures.push(created);
  lastInstanceId = created.id;

  return {
    ...created,
    result: {
      command: 'CreateInstance',
      operation_id: null,
      status: 'succeeded',
      safety: null,
      carriers: {},
      payload: {
        instance_id: created.id,
        queue_id: null,
        install_id: null,
        operation_id: null,
      },
      view_model: null,
    },
    view_model: {
      state_id: 'created',
      tone: 'success',
      title: 'Instance created',
      summary: `Created ${created.name}`,
      detail: null,
    },
  };
}

function createSourceOptions(): CreateOption[] {
  return [
    { id: 'vanilla', label: 'Vanilla', enabled: true },
    { id: FABRIC_COMPONENT_ID, label: 'Fabric', enabled: true },
    {
      id: 'net.minecraftforge',
      label: 'Forge',
      enabled: false,
      disabled_reason: 'Not included in mock fixtures.',
    },
    {
      id: 'net.neoforged',
      label: 'NeoForge',
      enabled: false,
      disabled_reason: 'Not included in mock fixtures.',
    },
    {
      id: 'org.quiltmc.quilt-loader',
      label: 'Quilt',
      enabled: false,
      disabled_reason: 'Not included in mock fixtures.',
    },
  ];
}

function createVersionRows(): CreateVersionRow[] {
  const vanillaRows = versionFixtures
    .filter((version) => !version.loader)
    .map((version): CreateVersionRow => {
      const channel = version.lifecycle.channel === 'legacy' ? 'legacy' : 'release';
      return {
        source_id: 'vanilla',
        selection_id: `vanilla|${version.id}`,
        minecraft_version_id: version.id,
        display_name: version.minecraft_meta.display_name || version.id,
        hint: version.minecraft_meta.display_hint || null,
        channel,
        tags: [{ id: 'release', label: 'Release' }],
        download_state: version.installed ? 'full' : 'none',
        create_enabled: true,
        disabled_reason: null,
      };
    });
  return [
    ...vanillaRows,
    {
      source_id: FABRIC_COMPONENT_ID,
      selection_id: `loader_version|${FABRIC_COMPONENT_ID}|${MOCK_FABRIC_MC_VERSION}`,
      minecraft_version_id: MOCK_FABRIC_MC_VERSION,
      display_name: MOCK_FABRIC_MC_VERSION,
      hint: null,
      channel: 'release',
      tags: [
        { id: 'stable', label: 'Stable' },
        { id: 'recommended', label: 'Recommended' },
      ],
      download_state: 'full',
      create_enabled: true,
      disabled_reason: null,
    },
  ];
}

function createPresetOptions(): CreatePresetOption[] {
  return [
    {
      id: '',
      label: 'Auto',
      detail: 'Axial picks safe JVM flags automatically.',
      default: true,
      disabled_reason: null,
    },
    {
      id: 'smooth',
      label: 'Smooth',
      detail: 'Balances throughput and steady frame times.',
      default: false,
      disabled_reason: null,
    },
    {
      id: 'performance',
      label: 'Performance',
      detail: 'Pushes higher throughput on modern hardware.',
      default: false,
      disabled_reason: null,
    },
  ];
}

function createSelection(selectionId: string): {
  versionId: string;
  versionDisplay: EnrichedInstance['version_display'];
  supportsMods: boolean;
} {
  const [kind, componentId, value] = selectionId.split('|');
  if (kind === 'vanilla') {
    const versionId = componentId ?? '';
    const version = versionFixtures.find((fixture) => !fixture.loader && fixture.id === versionId);
    if (!version) throw apiError(400, 'Bad Request', { error: 'unknown version selection' });
    return {
      versionId,
      versionDisplay: versionDisplay('vanilla', 'Vanilla', versionId, '', 'Vanilla', false),
      supportsMods: false,
    };
  }
  if (
    (kind === 'loader_version' && componentId === FABRIC_COMPONENT_ID && value === MOCK_FABRIC_MC_VERSION) ||
    (kind === 'loader_build' && componentId === FABRIC_COMPONENT_ID && value === MOCK_FABRIC_BUILD_ID)
  ) {
    return {
      versionId: MOCK_FABRIC_BUILD_ID,
      versionDisplay: versionDisplay(
        'fabric',
        'Fabric',
        MOCK_FABRIC_MC_VERSION,
        MOCK_FABRIC_LOADER_VERSION,
        `Fabric - ${MOCK_FABRIC_LOADER_VERSION}`,
        true,
      ),
      supportsMods: true,
    };
  }
  throw apiError(400, 'Bad Request', { error: 'unknown version selection' });
}

function versionDisplay(
  loaderKey: string,
  loaderLabel: string,
  minecraftLabel: string,
  loaderVersionLabel: string,
  loaderDetailLabel: string,
  supportsMods: boolean,
): EnrichedInstance['version_display'] {
  return {
    loader_key: loaderKey,
    loader_label: loaderLabel,
    minecraft_label: minecraftLabel,
    loader_version_label: loaderVersionLabel,
    loader_detail_label: loaderDetailLabel,
    summary_label: `${loaderLabel} - ${minecraftLabel}`,
    supports_mods: supportsMods,
  };
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

function cloneJsonResponse(value: unknown): unknown {
  if (value === undefined || value === null) return value;
  return JSON.parse(JSON.stringify(value));
}

function normalizeRequest(path: string): MockRequest {
  const url = new URL(path, 'http://axial.mock');
  const apiPrefix = '/api/v1';
  const unprefixed = url.pathname.startsWith(apiPrefix) ? url.pathname.slice(apiPrefix.length) || '/' : url.pathname;
  return {
    path: unprefixed.startsWith('/') ? unprefixed : `/${unprefixed}`,
    searchParams: url.searchParams,
  };
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
    java_path: input.java_path ?? '',
    window_width: input.window_width ?? 0,
    window_height: input.window_height ?? 0,
    jvm_preset: input.jvm_preset ?? '',
    performance_mode: input.performance_mode ?? '',
    extra_jvm_args: input.extra_jvm_args ?? '',
    icon: input.icon ?? '',
    accent: input.accent ?? '',
    version_display:
      input.version_display ?? versionDisplay('vanilla', 'Vanilla', input.version_id, '', 'Vanilla', false),
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

function uniqueInstanceId(name: string): string {
  const base = `mock-${slugify(name) || 'instance'}`;
  let id = base;
  let suffix = 2;
  while (instanceFixtures.some((instance) => instance.id === id)) {
    id = `${base}-${suffix}`;
    suffix += 1;
  }
  return id;
}

function slugify(value: string): string {
  return value
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 48);
}

function finiteNumber(value: unknown): number | undefined {
  return typeof value === 'number' && Number.isFinite(value) ? value : undefined;
}

function nextArtSeed(name: string): number {
  let hash = 2166136261;
  for (let i = 0; i < name.length; i += 1) {
    hash ^= name.charCodeAt(i);
    hash = Math.imul(hash, 16777619);
  }
  return Math.abs(hash) || 1;
}

function isRecord(value: unknown): value is Record<string, any> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}
