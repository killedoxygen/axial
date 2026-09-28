import {
  dtoArray,
  dtoBoolean,
  dtoEnum,
  dtoNumber,
  dtoOptionalNumber,
  dtoOptionalString,
  dtoRecord,
  dtoString,
} from './dto-contract';
import type { EnrichedInstance, Instance, InstanceResourceSummary } from './types-instance';
import type { Config, SystemInfo } from './types-settings';
import type { Version } from './types-version';

const JVM_PRESETS = [
  '',
  'smooth',
  'performance',
  'ultra_low_latency',
  'graalvm',
  'legacy',
  'legacy_pvp',
  'legacy_heavy',
] as const;
const PERFORMANCE_MODES = ['managed', 'vanilla', 'custom'] as const;
const GUARDIAN_MODES = ['managed', 'custom', 'disabled'] as const;
const THEMES = ['', 'obsidian', 'deepslate', 'nether', 'end', 'birch', 'custom'] as const;
const LOADER_IDS = [
  'net.fabricmc.fabric-loader',
  'org.quiltmc.quilt-loader',
  'net.minecraftforge',
  'net.neoforged',
] as const;
const LIFECYCLE_CHANNELS = ['stable', 'preview', 'experimental', 'legacy', 'unknown'] as const;
const LIFECYCLE_LABELS = [
  'release',
  'recommended',
  'latest',
  'snapshot',
  'pre_release',
  'release_candidate',
  'beta',
  'alpha',
  'old_beta',
  'old_alpha',
  'nightly',
  'dev',
  'unknown',
] as const;

function nullableNumber(value: unknown, label: string): number | null {
  return value == null ? null : dtoNumber(value, label);
}

function nullableBoolean(value: unknown, label: string): boolean | null {
  return value == null ? null : dtoBoolean(value, label);
}

export function configResponse(value: unknown): Config {
  const record = dtoRecord(value, 'Config');
  return {
    revision: dtoNumber(record.revision, 'Config revision'),
    username: dtoString(record.username, 'Config username'),
    launch_auth_mode: dtoEnum(record.launch_auth_mode, 'Config auth mode', ['offline', 'online'] as const),
    max_memory_mb: dtoNumber(record.max_memory_mb, 'Config maximum memory'),
    min_memory_mb: dtoNumber(record.min_memory_mb, 'Config minimum memory'),
    java_path_override: dtoString(record.java_path_override, 'Config Java path'),
    window_width: dtoNumber(record.window_width, 'Config window width'),
    window_height: dtoNumber(record.window_height, 'Config window height'),
    jvm_preset: dtoEnum(record.jvm_preset, 'Config JVM preset', JVM_PRESETS),
    performance_mode: dtoEnum(record.performance_mode, 'Config performance mode', PERFORMANCE_MODES),
    guardian_mode: dtoEnum(record.guardian_mode, 'Config guardian mode', GUARDIAN_MODES),
    guardian_idle_integrity_enabled: dtoBoolean(record.guardian_idle_integrity_enabled, 'Config Guardian integrity'),
    theme: dtoEnum(record.theme, 'Config theme', THEMES),
    custom_hue: nullableNumber(record.custom_hue, 'Config hue'),
    custom_vibrancy: nullableNumber(record.custom_vibrancy, 'Config vibrancy'),
    lightness: nullableNumber(record.lightness, 'Config lightness'),
    onboarding_done: dtoBoolean(record.onboarding_done, 'Config onboarding'),
    telemetry_enabled: dtoBoolean(record.telemetry_enabled, 'Config telemetry'),
    discord_rpc_enabled: dtoBoolean(record.discord_rpc_enabled, 'Config Discord RPC'),
    discord_rpc_onboarding_seen: dtoBoolean(record.discord_rpc_onboarding_seen, 'Config Discord onboarding'),
    music_enabled: nullableBoolean(record.music_enabled, 'Config music enabled'),
    music_volume: nullableNumber(record.music_volume, 'Config music volume'),
    music_track: dtoNumber(record.music_track, 'Config music track'),
  };
}

export function systemInfoResponse(value: unknown): SystemInfo {
  const record = dtoRecord(value, 'System info');
  return {
    total_memory_mb: dtoNumber(record.total_memory_mb, 'System total memory'),
    recommended_min_mb: dtoNumber(record.recommended_min_mb, 'System recommended minimum'),
    recommended_max_mb: dtoNumber(record.recommended_max_mb, 'System recommended maximum'),
    max_allocatable_gb: dtoNumber(record.max_allocatable_gb, 'System allocatable memory'),
  };
}

export function instanceResponse(value: unknown): Instance {
  const record = dtoRecord(value, 'Instance');
  const launchAction = record.launch_action == null ? undefined : launchActionResponse(record.launch_action);
  return {
    id: dtoString(record.id, 'Instance id'),
    name: dtoString(record.name, 'Instance name'),
    version_id: dtoString(record.version_id, 'Instance version'),
    created_at: dtoString(record.created_at, 'Instance creation time'),
    last_played_at: dtoOptionalString(record.last_played_at, 'Instance last played time'),
    art_seed: dtoOptionalNumber(record.art_seed, 'Instance art seed'),
    max_memory_mb: dtoOptionalNumber(record.max_memory_mb, 'Instance maximum memory'),
    min_memory_mb: dtoOptionalNumber(record.min_memory_mb, 'Instance minimum memory'),
    java_path: dtoOptionalString(record.java_path, 'Instance Java path'),
    window_width: dtoOptionalNumber(record.window_width, 'Instance window width'),
    window_height: dtoOptionalNumber(record.window_height, 'Instance window height'),
    jvm_preset: dtoOptionalString(record.jvm_preset, 'Instance JVM preset'),
    performance_mode:
      record.performance_mode == null
        ? undefined
        : dtoEnum(record.performance_mode, 'Instance performance mode', ['', ...PERFORMANCE_MODES] as const),
    extra_jvm_args: dtoOptionalString(record.extra_jvm_args, 'Instance JVM arguments'),
    icon: dtoOptionalString(record.icon, 'Instance icon'),
    accent: dtoOptionalString(record.accent, 'Instance accent'),
    launch_action: launchAction,
  };
}

function launchActionResponse(value: unknown): NonNullable<Instance['launch_action']> {
  const record = dtoRecord(value, 'Instance launch action');
  return {
    state_id: dtoString(record.state_id, 'Launch action state'),
    label: dtoString(record.label, 'Launch action label'),
    tone: dtoEnum(record.tone, 'Launch action tone', ['ok', 'warn', 'err', 'mute'] as const),
    launchable: dtoBoolean(record.launchable, 'Launch action availability'),
    primary_action: dtoEnum(record.primary_action, 'Launch primary action', ['launch', 'install', 'blocked'] as const),
    disabled_reason: dtoOptionalString(record.disabled_reason, 'Launch action disabled reason'),
  };
}

export function instancesResponse(value: unknown): { instances: EnrichedInstance[]; last_instance_id: string | null } {
  const record = dtoRecord(value, 'Instances');
  return {
    instances: dtoArray(record.instances, 'Instances list').map(enrichedInstanceResponse),
    last_instance_id: record.last_instance_id == null ? null : dtoString(record.last_instance_id, 'Last instance id'),
  };
}

export function enrichedInstanceResponse(value: unknown): EnrichedInstance {
  const record = dtoRecord(value, 'Enriched instance');
  const display = dtoRecord(record.version_display, 'Instance version display');
  return {
    ...instanceResponse(record),
    version_display: {
      loader_key: dtoString(display.loader_key, 'Instance loader key'),
      loader_label: dtoString(display.loader_label, 'Instance loader label'),
      minecraft_label: dtoString(display.minecraft_label, 'Instance Minecraft label'),
      loader_version_label: dtoString(display.loader_version_label, 'Instance loader version'),
      loader_detail_label: dtoString(display.loader_detail_label, 'Instance loader detail'),
      summary_label: dtoString(display.summary_label, 'Instance version summary'),
      supports_mods: dtoBoolean(display.supports_mods, 'Instance mod support'),
    },
    launchable: dtoBoolean(record.launchable, 'Instance launchable'),
    launch_action: launchActionResponse(record.launch_action),
    status_detail: dtoOptionalString(record.status_detail, 'Instance status detail'),
    needs_install: dtoOptionalString(record.needs_install, 'Instance install requirement'),
    java_major: dtoOptionalNumber(record.java_major, 'Instance Java major'),
    saves_count: dtoNumber(record.saves_count, 'Instance saves count'),
    mods_count: dtoNumber(record.mods_count, 'Instance mods count'),
    resource_count: dtoNumber(record.resource_count, 'Instance resource count'),
    shader_count: dtoNumber(record.shader_count, 'Instance shader count'),
  };
}

export function versionResponse(value: unknown): Version {
  const record = dtoRecord(value, 'Version');
  const minecraft = dtoRecord(record.minecraft_meta, 'Minecraft metadata');
  const lifecycle = dtoRecord(record.lifecycle, 'Version lifecycle');
  const loader = record.loader == null ? null : dtoRecord(record.loader, 'Version loader');
  return {
    subject_kind: dtoEnum(record.subject_kind, 'Version subject', ['installed_version', 'minecraft_version'] as const),
    id: dtoString(record.id, 'Version id'),
    raw_kind: dtoString(record.raw_kind, 'Version kind'),
    release_time: dtoOptionalString(record.release_time, 'Version release time'),
    minecraft_meta: {
      family: dtoString(minecraft.family, 'Minecraft family'),
      base_id: dtoString(minecraft.base_id, 'Minecraft base id'),
      effective_version: dtoString(minecraft.effective_version, 'Minecraft effective version'),
      variant_of: dtoString(minecraft.variant_of, 'Minecraft variant'),
      variant_kind: dtoString(minecraft.variant_kind, 'Minecraft variant kind'),
      display_name: dtoString(minecraft.display_name, 'Minecraft display name'),
      display_hint: dtoString(minecraft.display_hint, 'Minecraft display hint'),
    },
    lifecycle: {
      channel: dtoEnum(lifecycle.channel, 'Version lifecycle channel', LIFECYCLE_CHANNELS),
      labels: dtoArray(lifecycle.labels, 'Version lifecycle labels').map((label) =>
        dtoEnum(label, 'Version lifecycle label', LIFECYCLE_LABELS),
      ),
      default_rank: dtoNumber(lifecycle.default_rank, 'Version lifecycle rank'),
      badge_text: dtoString(lifecycle.badge_text, 'Version lifecycle badge'),
      provider_terms: dtoArray(lifecycle.provider_terms, 'Version provider terms').map((term) =>
        dtoString(term, 'Version provider term'),
      ),
    },
    inherits_from: dtoOptionalString(record.inherits_from, 'Version parent'),
    launchable: dtoBoolean(record.launchable, 'Version launchable'),
    installed: dtoBoolean(record.installed, 'Version installed'),
    status: dtoString(record.status, 'Version status'),
    status_detail: dtoOptionalString(record.status_detail, 'Version status detail'),
    needs_install: dtoOptionalString(record.needs_install, 'Version install requirement'),
    java_component: dtoOptionalString(record.java_component, 'Version Java component'),
    java_major: dtoOptionalNumber(record.java_major, 'Version Java major'),
    loader: loader
      ? {
          component_id: dtoEnum(loader.component_id, 'Loader component', LOADER_IDS),
          build_id: dtoString(loader.build_id, 'Loader build'),
          loader_version: dtoString(loader.loader_version, 'Loader version'),
        }
      : null,
  };
}

export function versionsResponse(value: unknown): { versions: Version[] } {
  const record = dtoRecord(value, 'Versions');
  return { versions: dtoArray(record.versions, 'Versions list').map(versionResponse) };
}

export function launcherStatusResponse(value: unknown): {
  dev_mode: boolean;
  setup_required: boolean;
  warnings: unknown;
} {
  const record = dtoRecord(value, 'Launcher status');
  return {
    dev_mode: dtoBoolean(record.dev_mode, 'Launcher development mode'),
    setup_required: dtoBoolean(record.setup_required, 'Launcher setup requirement'),
    warnings: record.warnings,
  };
}

export function musicStatusResponse(value: unknown): { count: number } {
  const record = dtoRecord(value, 'Music status');
  return { count: dtoNumber(record.count, 'Music track count') };
}

export function instanceResourcesResponse(value: unknown): InstanceResourceSummary {
  const record = dtoRecord(value, 'Instance resources');
  const file = (item: unknown, label: string): { name: string; size: number; modified_at: string } => {
    const entry = dtoRecord(item, label);
    return {
      name: dtoString(entry.name, `${label} name`),
      size: dtoNumber(entry.size, `${label} size`),
      modified_at: dtoString(entry.modified_at, `${label} modification time`),
    };
  };
  return {
    worlds: dtoArray(record.worlds, 'Instance worlds').map((item) => file(item, 'Instance world')),
    mods: dtoArray(record.mods, 'Instance mods').map((item) => {
      const entry = dtoRecord(item, 'Instance mod');
      return { ...file(entry, 'Instance mod'), enabled: dtoBoolean(entry.enabled, 'Instance mod enabled') };
    }),
    screenshots: dtoArray(record.screenshots, 'Instance screenshots').map((item) => file(item, 'Instance screenshot')),
    logs: dtoArray(record.logs, 'Instance logs').map((item) => file(item, 'Instance log')),
    worlds_count: dtoNumber(record.worlds_count, 'Instance world count'),
    mods_count: dtoNumber(record.mods_count, 'Instance mod count'),
    screenshots_count: dtoNumber(record.screenshots_count, 'Instance screenshot count'),
    logs_count: dtoNumber(record.logs_count, 'Instance log count'),
  };
}
