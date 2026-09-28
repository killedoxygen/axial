import type { LoaderComponentId } from '../../types-loader';
import type { CreateNotice } from '../../create-presenters';
import type { CreateLoaderBuildsView as CreateLoaderBuildsResponse } from '../../generated/CreateLoaderBuildsView';
import type { Channel } from './defaults';
import { dtoArray, dtoBoolean, dtoEnum, dtoNumber, dtoOptionalString, dtoRecord, dtoString } from '../../dto-contract';

export type CreateSourceId = 'vanilla' | LoaderComponentId;
export type { CreateLoaderBuildsResponse };

const LOADER_SOURCE_IDS = [
  'net.fabricmc.fabric-loader',
  'org.quiltmc.quilt-loader',
  'net.minecraftforge',
  'net.neoforged',
] as const;
const CREATE_SOURCE_IDS = ['vanilla', ...LOADER_SOURCE_IDS] as const;

export interface CreatePresetOption {
  id: string;
  label: string;
  detail: string;
  default: boolean;
  disabled_reason?: string | null;
}

export interface CreateOption<T extends string = string> {
  id: T;
  label: string;
  enabled: boolean;
  disabled_reason?: string | null;
}

export interface CreateVersionRow {
  source_id: CreateSourceId;
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

export interface CreateVersionTag {
  id: string;
  label: string;
}

interface CreateOptimizeOption {
  id: string;
  label: string;
  detail: string;
  default_enabled: boolean;
}

export interface CreateBackendViewResponse {
  sources: CreateOption<CreateSourceId>[];
  channels: CreateOption[];
  versions: CreateVersionRow[];
  preset_options: CreatePresetOption[];
  optimize_option: CreateOptimizeOption;
  notices: CreateNotice[];
  defaults: {
    source_id?: string;
    channel_id?: string;
    jvm_preset_id?: string;
    max_memory_mb?: number | null;
    window_width?: number | null;
    window_height?: number | null;
  };
}

function createSourceId(value: unknown, label: string): CreateSourceId {
  return dtoEnum(value, label, CREATE_SOURCE_IDS);
}

// Keep the retained Create filters while consuming the version owner's
// lifecycle vocabulary. Preview and experimental shared Snapshot in the
// baseline; unknown remains explicit rather than being inferred from a name.
function createChannel(value: string): Channel {
  switch (value) {
    case 'stable':
    case 'release':
      return 'release';
    case 'preview':
    case 'experimental':
    case 'snapshot':
      return 'snapshot';
    case 'legacy':
      return 'legacy';
    default:
      return 'unknown';
  }
}

export function createBackendViewResponse(value: unknown): CreateBackendViewResponse {
  const record = dtoRecord(value, 'Create view');
  const option = (item: unknown, label: string): CreateOption => {
    const entry = dtoRecord(item, label);
    return {
      id: dtoString(entry.id, `${label} id`),
      label: dtoString(entry.label, `${label} label`),
      enabled: dtoBoolean(entry.enabled, `${label} enabled`),
      disabled_reason:
        entry.disabled_reason == null ? null : dtoString(entry.disabled_reason, `${label} disabled reason`),
    };
  };
  const defaults = dtoRecord(record.defaults, 'Create defaults');
  const optimize = dtoRecord(record.optimize_option, 'Create optimize option');
  return {
    sources: dtoArray(record.sources, 'Create sources').map((item) => {
      const parsed = option(item, 'Create source');
      return { ...parsed, id: createSourceId(parsed.id, 'Create source id') };
    }),
    channels: dtoArray(record.channels, 'Create channels').map((item) => {
      const parsed = option(item, 'Create channel');
      return { ...parsed, id: createChannel(parsed.id) };
    }),
    versions: dtoArray(record.versions, 'Create versions').map(createVersionRowResponse),
    preset_options: dtoArray(record.preset_options, 'Create presets').map((item) => {
      const entry = dtoRecord(item, 'Create preset');
      return {
        id: dtoString(entry.id, 'Create preset id'),
        label: dtoString(entry.label, 'Create preset label'),
        detail: dtoString(entry.detail, 'Create preset detail'),
        default: dtoBoolean(entry.default, 'Create preset default'),
        disabled_reason:
          entry.disabled_reason == null ? null : dtoString(entry.disabled_reason, 'Create preset disabled reason'),
      };
    }),
    optimize_option: {
      id: dtoString(optimize.id, 'Create optimize id'),
      label: dtoString(optimize.label, 'Create optimize label'),
      detail: dtoString(optimize.detail, 'Create optimize detail'),
      default_enabled: dtoBoolean(optimize.default_enabled, 'Create optimize default'),
    },
    notices: (record.notices == null ? [] : dtoArray(record.notices, 'Create notices')).map((item) => {
      const entry = dtoRecord(item, 'Create notice');
      return {
        state_id: dtoString(entry.state_id, 'Create notice state'),
        tone: dtoString(entry.tone, 'Create notice tone'),
        message: dtoString(entry.message, 'Create notice message'),
        detail: entry.detail == null ? null : dtoString(entry.detail, 'Create notice detail'),
      };
    }),
    defaults: {
      source_id: defaults.source_id == null ? undefined : createSourceId(defaults.source_id, 'Create default source'),
      channel_id:
        defaults.channel_id == null
          ? undefined
          : createChannel(dtoString(defaults.channel_id, 'Create default channel')),
      jvm_preset_id: dtoOptionalString(defaults.jvm_preset_id, 'Create default preset'),
      max_memory_mb: defaults.max_memory_mb == null ? null : dtoNumber(defaults.max_memory_mb, 'Create default memory'),
      window_width: defaults.window_width == null ? null : dtoNumber(defaults.window_width, 'Create default width'),
      window_height: defaults.window_height == null ? null : dtoNumber(defaults.window_height, 'Create default height'),
    },
  };
}

function createVersionRowResponse(value: unknown): CreateVersionRow {
  const record = dtoRecord(value, 'Create version');
  return {
    source_id: createSourceId(record.source_id, 'Create version source'),
    selection_id: dtoString(record.selection_id, 'Create version selection'),
    minecraft_version_id: dtoString(record.minecraft_version_id, 'Create Minecraft version'),
    display_name: dtoString(record.display_name, 'Create version display name'),
    hint: record.hint == null ? null : dtoString(record.hint, 'Create version hint'),
    channel: createChannel(dtoString(record.channel, 'Create version channel')),
    tags: (record.tags == null ? [] : dtoArray(record.tags, 'Create version tags')).map((item) => {
      const tag = dtoRecord(item, 'Create version tag');
      return {
        id: dtoString(tag.id, 'Create version tag id'),
        label: dtoString(tag.label, 'Create version tag label'),
      };
    }),
    download_state: dtoString(record.download_state, 'Create version download state'),
    create_enabled: dtoBoolean(record.create_enabled, 'Create version enabled'),
    disabled_reason:
      record.disabled_reason == null ? null : dtoString(record.disabled_reason, 'Create disabled reason'),
  };
}

export function createLoaderBuildsResponse(
  value: unknown,
  expected?: { sourceId: CreateSourceId; minecraftVersionId: string },
): CreateLoaderBuildsResponse {
  const record = dtoRecord(value, 'Create loader builds');
  const auto = dtoRecord(record.auto, 'Create loader automatic option');
  if (
    expected &&
    (record.source_id !== expected.sourceId || record.minecraft_version_id !== expected.minecraftVersionId)
  ) {
    throw new Error('Loader options did not match the selected Minecraft version.');
  }
  return {
    source_id: dtoEnum(record.source_id, 'Create loader source', LOADER_SOURCE_IDS),
    minecraft_version_id: dtoString(record.minecraft_version_id, 'Create loader Minecraft version'),
    auto: {
      selection_id: dtoString(auto.selection_id, 'Create loader automatic selection'),
      label: dtoString(auto.label, 'Create loader automatic label'),
      detail: dtoString(auto.detail, 'Create loader automatic detail'),
      enabled: dtoBoolean(auto.enabled, 'Create loader automatic enabled'),
      disabled_reason:
        auto.disabled_reason === null
          ? null
          : dtoString(auto.disabled_reason, 'Create loader automatic disabled reason'),
    },
    builds: dtoArray(record.builds, 'Create loader builds').map((item) => {
      const build = dtoRecord(item, 'Create loader build');
      return {
        selection_id: dtoString(build.selection_id, 'Create loader selection'),
        build_id: dtoString(build.build_id, 'Create loader build id'),
        label: dtoString(build.label, 'Create loader build label'),
        channel_id: dtoString(build.channel_id, 'Create loader channel'),
        channel_label: dtoString(build.channel_label, 'Create loader channel label'),
        recommended: dtoBoolean(build.recommended, 'Create loader recommended'),
        installed: dtoBoolean(build.installed, 'Create loader installed'),
        enabled: dtoBoolean(build.enabled, 'Create loader enabled'),
        disabled_reason:
          build.disabled_reason == null ? null : dtoString(build.disabled_reason, 'Create loader disabled reason'),
      };
    }),
  };
}

export function createLoaderSelection(
  builds: CreateLoaderBuildsResponse | null,
  sourceId: CreateSourceId,
  minecraftVersionId: string | null,
  automaticSelection: string,
  pinnedSelection: string | null,
): string {
  if (sourceId === 'vanilla') return automaticSelection;
  if (!builds || builds.source_id !== sourceId || builds.minecraft_version_id !== minecraftVersionId) {
    return '';
  }
  if (!pinnedSelection) return builds.auto.enabled ? builds.auto.selection_id : '';
  return builds.builds.find((build) => build.selection_id === pinnedSelection && build.enabled)?.selection_id ?? '';
}
