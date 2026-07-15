import { modBaseName } from '../utils';
import type { Config, SystemInfo } from '../types-settings';
import type { EnrichedInstance, InstanceMod, InstanceResourceSummary } from '../types-instance';
import type { InstallQueueStateResponse } from '../types-install';
import type { FeatureFlagViewModel, FlagsResponse } from '../types-flags';
import type { UpdateFlowState, UpdateInfo } from '../types-update';
import type { Version } from '../types-version';
import type {
  CanonicalContent,
  ContentCompatResponse,
  ContentDetail,
  ContentKind,
  ContentPage,
  ContentSelection,
  ContentUpdatesResponse,
  InstanceContentEntry,
  InstanceContentResponse,
  ModpackFilesPlan,
  ModpackTarget,
  ResolutionPlan,
  SearchHit,
  TargetRef,
} from '../types-content';

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
  install_queue?: InstallQueueStateResponse;
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

console.info('Axial mock API active: data is fake, no backend running');

const missingHandlers = new Set<string>();
const flagOverrides = new Map<string, boolean>();

const FABRIC_COMPONENT_ID = 'net.fabricmc.fabric-loader';
const MOCK_FABRIC_MC_VERSION = '1.21.5';
const MOCK_FABRIC_MC_ALT = '1.21.4';
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

const MOCK_GAME_VERSIONS = ['1.20.1', '1.20.4', '1.21.1', '1.21.4', '1.21.5', '1.21.6'];

function daysAgo(days: number): string {
  return new Date(Date.now() - days * 86_400_000).toISOString();
}

interface MockVersionSeed {
  number: string;
  channel: 'release' | 'beta' | 'alpha';
  days: number;
  games: string[];
  size: number;
}

interface MockSeed {
  id: string;
  kind: ContentKind;
  title: string;
  author: string;
  summary: string;
  downloads: number;
  categories: string[];
  loaders?: string[];
  games?: string[];
  updatedDays: number;
  body: string;
  versions: MockVersionSeed[];
}

const MOCK_SEEDS: MockSeed[] = [
  {
    id: 'modrinth:AANobbMI',
    kind: 'mod',
    title: 'Sodium',
    author: 'jellysquid3',
    summary: 'A modern rendering engine that dramatically improves frame rates.',
    downloads: 182_000_000,
    categories: ['optimization', 'utility'],
    updatedDays: 6,
    body:
      '## What it does\n' +
      'Sodium replaces the vanilla chunk renderer with one built for modern GPUs. Most machines see two to three ' +
      'times the frame rate, and the gains are largest on integrated graphics.\n\n' +
      '### Highlights\n' +
      '- Rewritten chunk meshing and culling\n' +
      '- Fewer stalls when loading terrain\n' +
      '- Works alongside `Iris` for shader support\n\n' +
      '> Requires Fabric API. Not compatible with OptiFine.\n',
    versions: [
      { number: '0.7.3', channel: 'release', days: 6, games: ['1.21.6', '1.21.5'], size: 1_240_000 },
      { number: '0.7.2', channel: 'release', days: 41, games: ['1.21.5', '1.21.4'], size: 1_210_000 },
      { number: '0.7.0-beta.2', channel: 'beta', days: 78, games: ['1.21.4'], size: 1_180_000 },
      { number: '0.6.13', channel: 'release', days: 160, games: ['1.21.1'], size: 1_090_000 },
    ],
  },
  {
    id: 'modrinth:P7dR8mSH',
    kind: 'mod',
    title: 'Fabric API',
    author: 'modmuss50',
    summary: 'Core hooks and interoperability utilities most Fabric mods depend on.',
    downloads: 210_000_000,
    categories: ['library'],
    updatedDays: 3,
    body:
      'Fabric API is the shared library nearly every Fabric mod builds on. It provides the event hooks, registries ' +
      'and networking helpers the vanilla game does not expose.\n\n' +
      'You rarely install it on purpose; it comes along as a dependency of the mods that need it.\n',
    versions: [
      { number: '0.130.0', channel: 'release', days: 3, games: ['1.21.6'], size: 2_400_000 },
      { number: '0.129.0', channel: 'release', days: 24, games: ['1.21.5'], size: 2_380_000 },
      { number: '0.115.4', channel: 'release', days: 190, games: ['1.21.1'], size: 2_150_000 },
    ],
  },
  {
    id: 'modrinth:gvQqBUqZ',
    kind: 'mod',
    title: 'Lithium',
    author: 'jellysquid3',
    summary: 'General-purpose optimization mod that improves server tick performance.',
    downloads: 90_000_000,
    categories: ['optimization'],
    updatedDays: 15,
    body:
      'Lithium optimizes the game logic itself, from mob AI and block ticking to physics and chunk saving, without changing ' +
      'behaviour. Worlds tick faster and hitches on busy farms get shorter.\n\n' +
      '- No config needed\n' +
      '- Safe on servers and singleplayer\n' +
      '- Vanilla-identical results\n',
    versions: [
      { number: '0.17.2', channel: 'release', days: 15, games: ['1.21.6', '1.21.5'], size: 640_000 },
      { number: '0.16.3', channel: 'release', days: 96, games: ['1.21.4', '1.21.1'], size: 610_000 },
    ],
  },
  {
    id: 'modrinth:YL57xq9U',
    kind: 'mod',
    title: 'Iris Shaders',
    author: 'coderbot',
    summary: 'Run OptiFine-style shader packs on Fabric, with Sodium support.',
    downloads: 64_000_000,
    categories: ['optimization', 'decoration'],
    updatedDays: 9,
    body:
      'Iris loads the shader packs you already have and runs them on top of Sodium, so you keep the frame rate and ' +
      'get the lighting.\n\n### Notes\n- Pairs with any pack in the Shaders tab\n- Toggle shaders in-game without a restart\n',
    versions: [
      { number: '1.9.1', channel: 'release', days: 9, games: ['1.21.6'], size: 3_100_000 },
      { number: '1.8.8', channel: 'release', days: 70, games: ['1.21.4'], size: 3_020_000 },
    ],
  },
  {
    id: 'modrinth:mOgUt4GM',
    kind: 'mod',
    title: 'Mod Menu',
    author: 'Prospector',
    summary: 'Adds an in-game screen listing every mod you have, with their settings.',
    downloads: 48_000_000,
    categories: ['utility'],
    updatedDays: 21,
    body: 'A menu that lists the mods you have installed and opens their config screens. Most config-capable mods look for it.\n',
    versions: [
      { number: '15.0.0', channel: 'release', days: 21, games: ['1.21.6', '1.21.5'], size: 780_000 },
      { number: '14.0.0-rc.1', channel: 'alpha', days: 58, games: ['1.21.4'], size: 760_000 },
    ],
  },
  {
    id: 'modrinth:9eGKb6K1',
    kind: 'mod',
    title: 'Simple Voice Chat',
    author: 'henkelmax',
    summary: 'Proximity voice chat for vanilla-friendly servers, with no extra client needed.',
    downloads: 31_000_000,
    categories: ['social', 'utility'],
    updatedDays: 34,
    body: 'Adds distance-based voice chat. Players nearby are loud, players far away are faint, and groups get their own channel.\n',
    versions: [
      { number: '2.5.31', channel: 'release', days: 34, games: ['1.21.6', '1.21.5', '1.21.1'], size: 2_900_000 },
    ],
  },
  {
    id: 'modrinth:1KVo5zza',
    kind: 'modpack',
    title: 'Fabulously Optimized',
    author: 'robotkoer',
    summary: 'A performance-first pack that keeps the vanilla feel and adds quality-of-life polish.',
    downloads: 12_000_000,
    categories: ['optimization', 'lightweight'],
    updatedDays: 11,
    body:
      'Fabulously Optimized keeps Minecraft looking like Minecraft, but faster. It bundles the usual performance ' +
      'mods, sane defaults, and a handful of interface improvements.\n\n' +
      '### Included\n- Sodium, Lithium and friends\n- Iris for shader support\n- Zoom, better screenshots, fixed bugs\n',
    versions: [
      { number: '6.6.0', channel: 'release', days: 11, games: ['1.21.6'], size: 24_000_000 },
      { number: '6.5.1', channel: 'release', days: 62, games: ['1.21.5'], size: 23_400_000 },
    ],
  },
  {
    id: 'modrinth:MdwFAVRL',
    kind: 'modpack',
    title: 'Cobblemon',
    author: 'Cobblemon',
    summary: 'A creature-collecting adventure pack, built for Fabric.',
    downloads: 8_400_000,
    categories: ['adventure', 'multiplayer'],
    updatedDays: 28,
    body: 'Catch, train and battle creatures across the overworld. The pack ships with the mod, its models and a tuned world generator.\n',
    versions: [{ number: '1.6.1', channel: 'release', days: 28, games: ['1.21.1'], size: 41_000_000 }],
  },
  {
    id: 'modrinth:faithful32',
    kind: 'resource_pack',
    title: 'Faithful 32x',
    author: 'Faithful Team',
    summary: 'A higher-resolution take on the vanilla textures that keeps every shape intact.',
    downloads: 40_000_000,
    categories: ['32x', 'vanilla-like'],
    updatedDays: 19,
    body: 'Every vanilla texture, redrawn at double the resolution. Nothing is restyled, so blocks stay recognizable.\n',
    versions: [
      { number: '1.21.6a', channel: 'release', days: 19, games: ['1.21.6', '1.21.5'], size: 18_000_000 },
      { number: '1.21.4a', channel: 'release', days: 88, games: ['1.21.4', '1.21.1'], size: 17_600_000 },
    ],
  },
  {
    id: 'modrinth:barebones',
    kind: 'resource_pack',
    title: 'Bare Bones',
    author: 'RobotPants',
    summary: 'Flat, trailer-style textures with no noise and heavy saturation.',
    downloads: 9_200_000,
    categories: ['16x', 'simplistic'],
    updatedDays: 44,
    body: 'The look of the official trailers: flat colours, no grain, strong silhouettes.\n',
    versions: [
      { number: '1.21', channel: 'release', days: 44, games: ['1.21.6', '1.21.4', '1.20.1'], size: 4_100_000 },
    ],
  },
  {
    id: 'modrinth:complementary',
    kind: 'shader_pack',
    title: 'Complementary Shaders',
    author: 'EminGT',
    summary: 'A well-balanced shader pack with strong performance and a huge settings menu.',
    downloads: 25_000_000,
    categories: ['fantasy', 'reimagined'],
    updatedDays: 13,
    body:
      'Complementary aims for beauty that still runs. Volumetric light, soft shadows and water that reflects, with ' +
      'presets from potato to ultra.\n\nNeeds Iris or OptiFine to load.\n',
    versions: [
      { number: 'r5.5.1', channel: 'release', days: 13, games: ['1.21.6', '1.21.5', '1.21.1'], size: 6_800_000 },
      { number: 'r5.4.0', channel: 'release', days: 105, games: ['1.21.4'], size: 6_500_000 },
    ],
  },
  {
    id: 'modrinth:bsl',
    kind: 'shader_pack',
    title: 'BSL Shaders',
    author: 'CaptTatsu',
    summary: 'Warm, cinematic lighting with tone-mapped colour and soft reflections.',
    downloads: 21_500_000,
    categories: ['cartoon', 'semi-realistic'],
    updatedDays: 52,
    body: 'A long-running pack known for its warm palette and its bloom. Heavier than Complementary, prettier at sunset.\n',
    versions: [{ number: 'v8.4.01', channel: 'release', days: 52, games: ['1.21.6', '1.21.1'], size: 5_200_000 }],
  },
];

function seedContent(seed: MockSeed): CanonicalContent {
  const projectId = seed.id.replace('modrinth:', '');
  const loaders = seed.loaders ?? (seed.kind === 'mod' || seed.kind === 'modpack' ? ['fabric'] : []);
  return {
    canonical_id: seed.id,
    kind: seed.kind,
    provider: 'modrinth',
    project_id: projectId,
    slug: projectId,
    title: seed.title,
    author: seed.author,
    summary: seed.summary,
    downloads: seed.downloads,
    follows: Math.round(seed.downloads / 90),
    categories: seed.categories,
    game_versions: seed.games ?? MOCK_GAME_VERSIONS,
    loaders,
    updated: daysAgo(seed.updatedDays),
    sources: [{ provider: 'modrinth', project_id: projectId, slug: projectId }],
  };
}

const mockContentCatalog: Record<ContentKind, CanonicalContent[]> = {
  mod: MOCK_SEEDS.filter((seed) => seed.kind === 'mod').map(seedContent),
  modpack: MOCK_SEEDS.filter((seed) => seed.kind === 'modpack').map(seedContent),
  resource_pack: MOCK_SEEDS.filter((seed) => seed.kind === 'resource_pack').map(seedContent),
  shader_pack: MOCK_SEEDS.filter((seed) => seed.kind === 'shader_pack').map(seedContent),
};

const mockDependencies: Record<string, string[]> = {
  'modrinth:AANobbMI': ['modrinth:P7dR8mSH'],
  'modrinth:gvQqBUqZ': ['modrinth:P7dR8mSH'],
  'modrinth:YL57xq9U': ['modrinth:P7dR8mSH'],
  'modrinth:mOgUt4GM': ['modrinth:P7dR8mSH'],
};

const mockIncompatibilities: Record<string, string[]> = {
  'modrinth:9eGKb6K1': ['modrinth:gvQqBUqZ'],
};

function mockConflictsFor(canonicalIds: string[], installed: InstanceContentEntry[]) {
  return canonicalIds.flatMap((id) =>
    (mockIncompatibilities[id] ?? [])
      .filter((other) => installed.some((entry) => entry.canonical_id === other))
      .map((other) => ({
        canonical_id: id,
        kind: 'incompatible' as const,
        detail: `${mockCatalogItem(id)?.title ?? id} is incompatible with ${
          mockCatalogItem(other)?.title ?? other
        }, which is already installed`,
      })),
  );
}

const mockInstanceContent: Record<string, InstanceContentEntry[]> = {
  'mock-fabric-lab': [
    {
      canonical_id: 'modrinth:P7dR8mSH',
      title: 'Fabric API',
      kind: 'mod',
      provider: 'modrinth',
      project_id: 'P7dR8mSH',
      version_id: 'fabric-api-mock-1',
      filename: 'fabric-api.jar',
      enabled: true,
      source: 'managed',
    },
  ],
};

const mockLocalMods: Record<string, InstanceMod[]> = {
  'mock-fabric-lab': [
    { name: 'localcraft-tweaks-1.2.3.jar', size: 182_000, modified_at: daysAgo(30), enabled: true },
    { name: 'handmade-patch.jar.disabled', size: 96_000, modified_at: daysAgo(45), enabled: false },
  ],
};

function mockCatalogItem(canonicalId: string): CanonicalContent | undefined {
  return Object.values(mockContentCatalog)
    .flat()
    .find((item) => item.canonical_id === canonicalId);
}

function mockContentSearch(request: MockRequest | undefined): ContentPage {
  const kind = (request?.searchParams.get('kind') as ContentKind | null) ?? 'mod';
  const query = (request?.searchParams.get('query') ?? '').toLowerCase();
  const categoryFilter = request?.searchParams.get('category') ?? '';
  const instanceId = request?.searchParams.get('instance_id') ?? '';
  const installed = new Set((mockInstanceContent[instanceId] ?? []).map((entry) => entry.canonical_id));
  const items: SearchHit[] = (mockContentCatalog[kind] ?? [])
    .filter((item) => !query || item.title.toLowerCase().includes(query) || item.summary.toLowerCase().includes(query))
    .filter((item) => !categoryFilter || item.categories.includes(categoryFilter))
    .map((item) => (installed.has(item.canonical_id) ? { ...item, install_state: 'installed' as const } : item));
  return { items, offset: 0, limit: items.length, total: items.length };
}

function mockContentDetail(request: MockRequest | undefined): ContentDetail {
  const id = request?.searchParams.get('id') ?? '';
  const seed = MOCK_SEEDS.find((entry) => entry.id === id);
  const item = mockCatalogItem(id);
  if (!seed || !item) throw apiError(404, 'Not Found', { error: 'content not found' });

  const dependencies = (mockDependencies[item.canonical_id] ?? []).map((projectId) => ({
    project_id: projectId.replace('modrinth:', ''),
    kind: 'required' as const,
  }));
  const extension = seed.kind === 'mod' ? 'jar' : seed.kind === 'modpack' ? 'mrpack' : 'zip';

  return {
    ...item,
    body: seed.body,
    gallery: [],
    versions: seed.versions.map((version, index) => ({
      id: `${item.project_id}-${version.number}`,
      name: `${seed.title} ${version.number}`,
      version_number: version.number,
      game_versions: version.games,
      loaders: item.loaders,
      channel: version.channel,
      published: daysAgo(version.days),
      downloads: Math.round(seed.downloads / (index + 1) / 3),
      files: [
        {
          url: `https://example.invalid/${item.project_id}-${version.number}.${extension}`,
          filename: `${item.project_id}-${version.number}.${extension}`,
          primary: true,
          size: version.size,
        },
      ],
      dependencies,
    })),
  };
}

function mockResolvePlan(instanceId: string | undefined, selections: ContentSelection[]): ResolutionPlan {
  const installed = instanceId ? (mockInstanceContent[instanceId] ?? []) : [];
  const seen = new Set<string>();
  const queue = [...selections.map((selection) => ({ id: selection.canonical_id, dependency: false }))];
  for (const selection of selections) {
    for (const dep of mockDependencies[selection.canonical_id] ?? []) {
      queue.push({ id: dep, dependency: true });
    }
  }
  const items = queue
    .filter((entry) => {
      if (seen.has(entry.id)) return false;
      seen.add(entry.id);
      return true;
    })
    .map((entry) => {
      const item = mockCatalogItem(entry.id);
      const seed = MOCK_SEEDS.find((candidate) => candidate.id === entry.id);
      const latest = seed?.versions[0];
      const already = installed.some((installedEntry) => installedEntry.canonical_id === entry.id);
      const projectId = entry.id.replace('modrinth:', '');
      const extension = seed?.kind === 'resource_pack' || seed?.kind === 'shader_pack' ? 'zip' : 'jar';
      return {
        canonical_id: entry.id,
        title: item?.title ?? entry.id,
        kind: item?.kind ?? ('mod' as ContentKind),
        project_id: projectId,
        version_id: `${projectId}-${latest?.number ?? '1.0.0'}`,
        version_number: latest?.number ?? '1.0.0',
        filename: `${projectId}-${latest?.number ?? '1.0.0'}.${extension}`,
        size: latest?.size ?? 512_000,
        dependencies: [],
        reason: entry.dependency ? ('dependency' as const) : ('selected' as const),
        already_installed: already,
        update: false,
      };
    });
  const totalBytes = items
    .filter((item) => !item.already_installed || item.update)
    .reduce((sum, item) => sum + (item.size ?? 0), 0);
  return {
    instance_id: instanceId,
    loader: 'fabric',
    game_version: '1.21.6',
    items,
    conflicts: mockConflictsFor(
      items.map((item) => item.canonical_id),
      installed,
    ),
    total_download_bytes: totalBytes,
  };
}

function mockContentCompatibility(selections: ContentSelection[]): ContentCompatResponse {
  if (selections.length === 0) return { candidates: [] };
  const total = selections.length;
  return {
    candidates: [
      {
        loader: 'fabric',
        loader_label: 'Fabric',
        game_version: MOCK_FABRIC_MC_VERSION,
        selection_id: `loader_version|${FABRIC_COMPONENT_ID}|${MOCK_FABRIC_MC_VERSION}`,
        summary: total === 1 ? 'Works here' : `All ${total} work here`,
        supported_count: total,
        total_count: total,
        complete: true,
        drops: [],
      },
      {
        loader: 'fabric',
        loader_label: 'Fabric',
        game_version: MOCK_FABRIC_MC_ALT,
        selection_id: `loader_version|${FABRIC_COMPONENT_ID}|${MOCK_FABRIC_MC_ALT}`,
        summary: `${Math.max(1, total - 1)} of ${total} work here`,
        supported_count: Math.max(1, total - 1),
        total_count: total,
        complete: total === 1,
        drops:
          total > 1
            ? [
                {
                  canonical_id: selections[total - 1].canonical_id,
                  title: mockCatalogItem(selections[total - 1].canonical_id)?.title ?? 'an item',
                },
              ]
            : [],
      },
    ],
  };
}

function mockModpackTarget(canonicalId: string): ModpackTarget {
  const item = mockCatalogItem(canonicalId);
  if (!item) throw apiError(404, 'Not Found', { error: 'content not found' });
  return {
    canonical_id: canonicalId,
    version_id: `${item.project_id}-v1`,
    name: item.title,
    minecraft: MOCK_FABRIC_MC_VERSION,
    loader: 'fabric',
    loader_label: 'Fabric',
    selection_id: `loader_build|${FABRIC_COMPONENT_ID}|${MOCK_FABRIC_BUILD_ID}`,
  };
}

function mockModpackInstall(body: unknown): InstallQueueStateResponse {
  if (!isRecord(body) || typeof body.instance_id !== 'string' || typeof body.canonical_id !== 'string') {
    throw apiError(400, 'Bad Request', { error: 'invalid modpack install request' });
  }
  return mockEmptyInstallQueue('Content queued', 'Modpack files');
}

function mockContentInstall(body: unknown): InstallQueueStateResponse {
  if (!isRecord(body) || typeof body.instance_id !== 'string' || !Array.isArray(body.selections)) {
    throw apiError(400, 'Bad Request', { error: 'invalid install request' });
  }
  const instanceId = body.instance_id;
  const plan = mockResolvePlan(instanceId, body.selections as ContentSelection[]);
  const hasUnavailable = plan.conflicts.some((conflict) => conflict.kind === 'unavailable');
  if (plan.conflicts.length > 0 && (hasUnavailable || body.allow_incompatible !== true)) {
    throw apiError(409, 'Conflict', {
      error: plan.conflicts.map((conflict) => conflict.detail).join('; '),
      conflicts: plan.conflicts,
    });
  }
  const current = mockInstanceContent[instanceId] ?? (mockInstanceContent[instanceId] = []);
  for (const item of plan.items) {
    const existing = current.find((entry) => entry.canonical_id === item.canonical_id);
    if (existing) {
      existing.version_id = item.version_id;
      existing.filename = item.filename;
      continue;
    }
    current.push({
      canonical_id: item.canonical_id,
      title: item.title,
      kind: item.kind,
      provider: 'modrinth',
      project_id: item.project_id,
      version_id: item.version_id,
      filename: item.filename,
      enabled: true,
      source: 'managed',
    });
  }
  return mockEmptyInstallQueue('Content queued', `${plan.items.length} items`);
}

function mockInstanceContentList(instanceId: string): InstanceContentResponse {
  return { entries: mockInstanceContent[instanceId] ?? [] };
}

function mockContentUninstall(instanceId: string, canonicalId: string): InstallQueueStateResponse {
  return mockContentUninstalls(instanceId, [canonicalId]);
}

function mockContentUninstalls(instanceId: string, canonicalIds: string[]): InstallQueueStateResponse {
  const selected = new Set(canonicalIds);
  const current = mockInstanceContent[instanceId] ?? [];
  mockInstanceContent[instanceId] = current.filter((entry) => !selected.has(entry.canonical_id));
  return mockEmptyInstallQueue('Content queued', 'Removal');
}

function mockEmptyInstallQueue(message?: string, detail?: string): InstallQueueStateResponse {
  return {
    active: null,
    items: [],
    view_model: {
      state_id: 'idle',
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
    notice: message ? { state_id: 'queued', tone: 'info', message, detail } : null,
    started_install: null,
  };
}

function mockModpackFiles(_instanceId: string, canonicalId: string): ModpackFilesPlan {
  const item = mockCatalogItem(canonicalId);
  if (!item) throw apiError(404, 'Not Found', { error: 'content not found' });
  return {
    canonical_id: canonicalId,
    version_id: `${item.project_id}-v1`,
    name: item.title,
    minecraft: MOCK_FABRIC_MC_VERSION,
    loader: 'fabric',
    files: [
      {
        path: 'mods/sodium.jar',
        filename: 'sodium.jar',
        kind: 'mod',
        size: 1_250_000,
        title: 'Sodium',
        identified: true,
        compatible: true,
        installed: false,
      },
      {
        path: 'mods/lithium.jar',
        filename: 'lithium.jar',
        kind: 'mod',
        size: 740_000,
        title: 'Lithium',
        identified: true,
        compatible: true,
        installed: false,
      },
    ],
  };
}

function mockContentUpdates(instanceId: string): ContentUpdatesResponse {
  const installed = mockInstanceContent[instanceId] ?? [];
  const updates = installed.flatMap((entry) => {
    if (entry.kind === 'modpack') return [];
    const seed = MOCK_SEEDS.find((candidate) => candidate.id === entry.canonical_id);
    const latest = seed?.versions[0];
    if (!latest) return [];
    if (mockConflictsFor([entry.canonical_id], installed).length > 0) return [];
    const latestId = `${entry.project_id}-${latest.number}`;
    if (latestId === entry.version_id) return [];
    return [
      {
        canonical_id: entry.canonical_id,
        title: entry.title,
        kind: entry.kind,
        current_version_id: entry.version_id,
        latest_version_id: latestId,
        latest_version_number: latest.number,
      },
    ];
  });
  return { updates };
}

function mockInstanceResources(instanceId: string): InstanceResourceSummary {
  findInstance(instanceId);
  const contentMods = (mockInstanceContent[instanceId] ?? [])
    .filter((entry) => entry.kind === 'mod')
    .map((entry, index) => ({
      name: entry.enabled ? entry.filename : `${entry.filename}.disabled`,
      size: 240_000 + index * 87_000,
      modified_at: daysAgo(index + 2),
      enabled: entry.enabled,
    }));
  const mods = [...contentMods, ...(mockLocalMods[instanceId] ?? [])].sort((a, b) =>
    a.name.toLowerCase().localeCompare(b.name.toLowerCase()),
  );
  return {
    worlds: [],
    mods,
    screenshots: [],
    logs: [],
    worlds_count: 0,
    mods_count: mods.length,
    screenshots_count: 0,
    logs_count: 0,
  };
}

function mockSetModEnabled(instanceId: string, name: string, body: unknown): unknown {
  const enabled = isRecord(body) && body.enabled === true;
  const base = modBaseName(name);
  const entry = (mockInstanceContent[instanceId] ?? []).find((candidate) => candidate.filename === base);
  if (entry) {
    entry.enabled = enabled;
    return { status: 'ok', name: enabled ? base : `${base}.disabled`, enabled };
  }
  const local = (mockLocalMods[instanceId] ?? []).find((mod) => modBaseName(mod.name) === base);
  if (!local) throw apiError(404, 'Not Found', { error: 'unknown mod' });
  local.enabled = enabled;
  local.name = enabled ? base : `${base}.disabled`;
  return { status: 'ok', name: local.name, enabled };
}

function mockDeleteMod(instanceId: string, name: string): unknown {
  const base = modBaseName(name);
  mockInstanceContent[instanceId] = (mockInstanceContent[instanceId] ?? []).filter((entry) => entry.filename !== base);
  mockLocalMods[instanceId] = (mockLocalMods[instanceId] ?? []).filter((mod) => modBaseName(mod.name) !== base);
  return { status: 'ok' };
}

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

const mockSetupPlans = new Map<string, ContentSelection[]>();

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
  'POST /instances/modpack': (body): CreateInstanceResponse => {
    const created = createInstance(body);
    return {
      ...created,
      install_queue: mockEmptyInstallQueue('Content queued', 'Modpack setup'),
    };
  },
  'POST /instances/setup/plan': (body) => {
    if (
      !isRecord(body) ||
      typeof body.selection_id !== 'string' ||
      !isRecord(body.target) ||
      !Array.isArray(body.selections)
    ) {
      throw apiError(400, 'Bad Request', { error: 'invalid setup plan request' });
    }
    const selections = body.selections as ContentSelection[];
    const plan = mockResolvePlan(undefined, selections);
    const planId = plan.conflicts.length === 0 ? `mock-setup-${Date.now()}` : undefined;
    if (planId) mockSetupPlans.set(planId, selections);
    return {
      ...(planId ? { plan_id: planId } : {}),
      expires_at_ms: Date.now() + 5 * 60_000,
      selection_id: body.selection_id,
      plan,
    };
  },
  'POST /instances/setup': (body): CreateInstanceResponse => {
    if (!isRecord(body) || typeof body.plan_id !== 'string') {
      throw apiError(400, 'Bad Request', { error: 'invalid setup request' });
    }
    const selections = mockSetupPlans.get(body.plan_id);
    if (!selections) throw apiError(409, 'Conflict', { error: 'Setup plan is missing or expired.' });
    mockSetupPlans.delete(body.plan_id);
    const created = createInstance(body);
    mockContentInstall({ instance_id: created.id, selections, allow_incompatible: false });
    return created;
  },
  'GET /install/queue': (): InstallQueueStateResponse => mockEmptyInstallQueue(),
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
  'GET /content/search': (_body, _path, request) => mockContentSearch(request),
  'GET /content/item': (_body, _path, request) => mockContentDetail(request),
  'POST /content/plan': (body) => {
    if (!isRecord(body) || !isRecord(body.target) || !Array.isArray(body.selections)) {
      throw apiError(400, 'Bad Request', { error: 'invalid plan request' });
    }
    const target = body.target as TargetRef;
    const instanceId = target.kind === 'instance' ? target.instance_id : undefined;
    return mockResolvePlan(instanceId, body.selections as ContentSelection[]);
  },
  'POST /content/install': (body) => mockContentInstall(body),
  'POST /content/compatibility': (body) => {
    if (!isRecord(body) || !Array.isArray(body.selections)) {
      throw apiError(400, 'Bad Request', { error: 'invalid compatibility request' });
    }
    return mockContentCompatibility(body.selections as ContentSelection[]);
  },
  'GET /content/modpack/target': (_body, _path, request) => mockModpackTarget(request?.searchParams.get('id') ?? ''),
  'GET /content/modpack/files': (_body, _path, request) =>
    mockModpackFiles(request?.searchParams.get('instance_id') ?? '', request?.searchParams.get('id') ?? ''),
  'POST /content/modpack/install': (body) => mockModpackInstall(body),
  'GET /instances/{id}/content': (_body, path) => mockInstanceContentList(instanceIdFromContentPath(path)),
  'GET /instances/{id}/content/updates': (_body, path) => mockContentUpdates(instanceIdFromSubPath(path)),
  'GET /instances/{id}/resources': (_body, path) => mockInstanceResources(instanceIdFromSubPath(path)),
  'PUT /instances/{id}/mods/{name}': (body, path) =>
    mockSetModEnabled(instanceIdFromSubPath(path), modNameFromPath(path), body),
  'DELETE /instances/{id}/mods/{name}': (_body, path) =>
    mockDeleteMod(instanceIdFromSubPath(path), modNameFromPath(path)),
  'DELETE /instances/{id}/content': (_body, path, request) =>
    mockContentUninstall(instanceIdFromContentPath(path), request?.searchParams.get('id') ?? ''),
  'POST /instances/{id}/content/uninstall': (body, path) => {
    if (!isRecord(body) || !Array.isArray(body.canonical_ids)) {
      throw apiError(400, 'Bad Request', { error: 'invalid uninstall request' });
    }
    return mockContentUninstalls(
      instanceIdFromSubPath(path),
      body.canonical_ids.filter((id): id is string => typeof id === 'string'),
    );
  },
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
  const exact = `${method} ${path}`;
  if (Object.prototype.hasOwnProperty.call(handlers, exact)) return exact;
  if (method === 'PUT' && path.startsWith('/flags/')) return 'PUT /flags/{key}';
  if (/^\/instances\/[^/]+\/content$/.test(path)) return `${method} /instances/{id}/content`;
  if (/^\/instances\/[^/]+\/content\/uninstall$/.test(path)) return `${method} /instances/{id}/content/uninstall`;
  if (/^\/instances\/[^/]+\/content\/updates$/.test(path)) return `${method} /instances/{id}/content/updates`;
  if (/^\/instances\/[^/]+\/resources$/.test(path)) return `${method} /instances/{id}/resources`;
  if (/^\/instances\/[^/]+\/mods\/[^/]+$/.test(path)) return `${method} /instances/{id}/mods/{name}`;
  if (/^\/instances\/[^/]+$/.test(path) && path !== '/instances/create-view') return `${method} /instances/{id}`;
  return `${method} ${path}`;
}

function instanceIdFromPath(path: string | undefined): string {
  return decodeURIComponent((path ?? '').slice('/instances/'.length));
}

function instanceIdFromContentPath(path: string | undefined): string {
  const match = /^\/instances\/([^/]+)\/content$/.exec(path ?? '');
  return match ? decodeURIComponent(match[1]) : '';
}

function instanceIdFromSubPath(path: string | undefined): string {
  const match = /^\/instances\/([^/]+)\//.exec(path ?? '');
  return match ? decodeURIComponent(match[1]) : '';
}

function modNameFromPath(path: string | undefined): string {
  const match = /^\/instances\/[^/]+\/mods\/([^/]+)$/.exec(path ?? '');
  return match ? decodeURIComponent(match[1]) : '';
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
  if (minecraftVersion !== MOCK_FABRIC_MC_VERSION && minecraftVersion !== MOCK_FABRIC_MC_ALT) {
    throw apiError(404, 'Not Found', { error: 'no mock loader builds for this Minecraft version' });
  }
  return {
    source_id: FABRIC_COMPONENT_ID,
    minecraft_version_id: minecraftVersion,
    auto: {
      selection_id: `loader_version|${FABRIC_COMPONENT_ID}|${minecraftVersion}`,
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
    {
      source_id: FABRIC_COMPONENT_ID,
      selection_id: `loader_version|${FABRIC_COMPONENT_ID}|${MOCK_FABRIC_MC_ALT}`,
      minecraft_version_id: MOCK_FABRIC_MC_ALT,
      display_name: MOCK_FABRIC_MC_ALT,
      hint: null,
      channel: 'release',
      tags: [{ id: 'stable', label: 'Stable' }],
      download_state: 'none',
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
  const [kind, rawComponentId, value] = selectionId.split('|');
  const componentId = rawComponentId === 'fabric' ? FABRIC_COMPONENT_ID : rawComponentId;
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
    (kind === 'loader_version' &&
      componentId === FABRIC_COMPONENT_ID &&
      (value === MOCK_FABRIC_MC_VERSION || value === MOCK_FABRIC_MC_ALT)) ||
    (kind === 'loader_build' && componentId === FABRIC_COMPONENT_ID && value === MOCK_FABRIC_BUILD_ID)
  ) {
    const minecraft = kind === 'loader_version' ? value : MOCK_FABRIC_MC_VERSION;
    return {
      versionId: MOCK_FABRIC_BUILD_ID,
      versionDisplay: versionDisplay(
        'fabric',
        'Fabric',
        minecraft,
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
