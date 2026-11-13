import {
  dtoArray,
  dtoBoolean,
  dtoEnum,
  dtoNumber,
  dtoOptionalNumber,
  dtoOptionalString,
  dtoRecord,
  dtoString,
  isDtoRecord,
} from './dto-contract';
import type {
  CanonicalContent,
  ContentCompatResponse,
  ContentDependency,
  ContentDetail,
  ContentPage,
  ContentUpdatesResponse,
  InstanceContentResponse,
  InstanceSetupPlanResponse,
  ModpackFilesPlan,
  ModpackTarget,
  RequiredRefusal,
  ResolutionPlan,
  SearchHit,
} from './types-content';

const CONTENT_KINDS = ['mod', 'modpack', 'resource_pack', 'shader_pack'] as const;
const RELEASE_CHANNELS = ['release', 'beta', 'alpha'] as const;
const LOADER_KEYS = ['vanilla', 'fabric', 'quilt', 'forge', 'neoforge'] as const;

function strings(value: unknown, label: string): string[] {
  return dtoArray(value, label).map((item) => dtoString(item, label));
}

function canonicalContentResponse(value: unknown): CanonicalContent {
  const record = dtoRecord(value, 'Content item');
  return {
    canonical_id: dtoString(record.canonical_id, 'Content id'),
    kind: dtoEnum(record.kind, 'Content kind', CONTENT_KINDS),
    provider: dtoEnum(record.provider, 'Content provider', ['modrinth'] as const),
    project_id: dtoString(record.project_id, 'Content project'),
    slug: dtoOptionalString(record.slug, 'Content slug'),
    title: dtoString(record.title, 'Content title'),
    author: dtoString(record.author, 'Content author'),
    summary: dtoString(record.summary, 'Content summary'),
    icon_url: dtoOptionalString(record.icon_url, 'Content icon'),
    cover_url: dtoOptionalString(record.cover_url, 'Content cover'),
    downloads: dtoNumber(record.downloads, 'Content downloads'),
    follows: dtoNumber(record.follows, 'Content follows'),
    categories: strings(record.categories, 'Content categories'),
    game_versions: strings(record.game_versions, 'Content game versions'),
    loaders: strings(record.loaders, 'Content loaders'),
    updated: dtoOptionalString(record.updated, 'Content update time'),
  };
}

function dependencyResponse(value: unknown): ContentDependency {
  const record = dtoRecord(value, 'Content dependency');
  return {
    project_id: dtoOptionalString(record.project_id, 'Content dependency project'),
    version_id: dtoOptionalString(record.version_id, 'Content dependency version'),
    kind: dtoEnum(record.kind, 'Content dependency kind', [
      'required',
      'optional',
      'incompatible',
      'embedded',
    ] as const),
  };
}

export function contentPageResponse(value: unknown): ContentPage {
  const record = dtoRecord(value, 'Content page');
  return {
    items: dtoArray(record.items, 'Content results').map((item): SearchHit => {
      const parsed = canonicalContentResponse(item);
      const source = dtoRecord(item, 'Content result');
      return {
        ...parsed,
        install_state:
          source.install_state == null
            ? undefined
            : dtoEnum(source.install_state, 'Content install state', ['installed'] as const),
      };
    }),
    offset: dtoNumber(record.offset, 'Content offset'),
    limit: dtoNumber(record.limit, 'Content limit'),
    total: dtoNumber(record.total, 'Content total'),
  };
}

export function contentDetailResponse(value: unknown): ContentDetail {
  const record = dtoRecord(value, 'Content detail');
  return {
    ...canonicalContentResponse(record),
    body: dtoString(record.body, 'Content body'),
    gallery: dtoArray(record.gallery, 'Content gallery').map((image) => {
      const item = dtoRecord(image, 'Content gallery image');
      return { url: dtoString(item.url, 'Gallery image URL'), title: dtoOptionalString(item.title, 'Gallery title') };
    }),
    versions: dtoArray(record.versions, 'Content versions').map((version) => {
      const item = dtoRecord(version, 'Content version');
      return {
        id: dtoString(item.id, 'Content version id'),
        name: dtoString(item.name, 'Content version name'),
        version_number: dtoString(item.version_number, 'Content version number'),
        game_versions: strings(item.game_versions, 'Content version game versions'),
        loaders: strings(item.loaders, 'Content version loaders'),
        channel: dtoEnum(item.channel, 'Content release channel', RELEASE_CHANNELS),
        published: dtoOptionalString(item.published, 'Content publication time'),
        downloads: dtoNumber(item.downloads, 'Content version downloads'),
        files: dtoArray(item.files, 'Content files').map((file) => {
          const entry = dtoRecord(file, 'Content file');
          return {
            url: dtoString(entry.url, 'Content file URL'),
            filename: dtoString(entry.filename, 'Content filename'),
            sha1: dtoOptionalString(entry.sha1, 'Content SHA-1'),
            sha512: dtoOptionalString(entry.sha512, 'Content SHA-512'),
            size: dtoOptionalNumber(entry.size, 'Content file size'),
            primary: dtoBoolean(entry.primary, 'Content primary file'),
          };
        }),
        dependencies: dtoArray(item.dependencies, 'Content dependencies').map(dependencyResponse),
      };
    }),
  };
}

export function resolutionPlanResponse(value: unknown): ResolutionPlan {
  const record = dtoRecord(value, 'Content plan');
  return {
    instance_id: dtoOptionalString(record.instance_id, 'Content plan instance'),
    loader: dtoString(record.loader, 'Content plan loader'),
    game_version: dtoString(record.game_version, 'Content plan game version'),
    items: dtoArray(record.items, 'Content plan items').map((item) => {
      const entry = dtoRecord(item, 'Content plan item');
      return {
        canonical_id: dtoString(entry.canonical_id, 'Content plan item id'),
        title: dtoString(entry.title, 'Content plan item title'),
        kind: dtoEnum(entry.kind, 'Content plan item kind', CONTENT_KINDS),
        project_id: dtoString(entry.project_id, 'Content plan project'),
        version_id: dtoString(entry.version_id, 'Content plan version'),
        version_number: dtoString(entry.version_number, 'Content plan version number'),
        filename: dtoString(entry.filename, 'Content plan filename'),
        sha1: dtoOptionalString(entry.sha1, 'Content plan SHA-1'),
        sha512: dtoOptionalString(entry.sha512, 'Content plan SHA-512'),
        size: dtoOptionalNumber(entry.size, 'Content plan size'),
        dependencies:
          entry.dependencies === undefined
            ? []
            : dtoArray(entry.dependencies, 'Content plan dependencies').map(dependencyResponse),
        reason: dtoEnum(entry.reason, 'Content plan reason', ['selected', 'dependency'] as const),
        already_installed: dtoBoolean(entry.already_installed, 'Content already installed'),
        update: dtoBoolean(entry.update, 'Content update'),
        existing_file:
          entry.existing_file === undefined
            ? undefined
            : dtoEnum(entry.existing_file, 'Content existing file', ['identical', 'different'] as const),
      };
    }),
    conflicts: dtoArray(record.conflicts, 'Content conflicts').map((conflict) => {
      const entry = dtoRecord(conflict, 'Content conflict');
      return {
        canonical_id: dtoOptionalString(entry.canonical_id, 'Content conflict id'),
        title: dtoOptionalString(entry.title, 'Content conflict title'),
        kind: dtoEnum(entry.kind, 'Content conflict kind', ['unavailable', 'incompatible', 'existing_file'] as const),
        detail: dtoString(entry.detail, 'Content conflict detail'),
        path: dtoOptionalString(entry.path, 'Content conflict path'),
      };
    }),
    total_download_bytes: dtoNumber(record.total_download_bytes, 'Content download bytes'),
  };
}

export function instanceSetupPlanResponse(value: unknown): InstanceSetupPlanResponse {
  const record = dtoRecord(value, 'Instance setup plan');
  return {
    plan_id: dtoOptionalString(record.plan_id, 'Instance setup plan id'),
    expires_at_ms: dtoNumber(record.expires_at_ms, 'Instance setup expiry'),
    selection_id: dtoString(record.selection_id, 'Instance setup selection'),
    plan: resolutionPlanResponse(record.plan),
  };
}

export function contentCompatResponse(value: unknown): ContentCompatResponse {
  const record = dtoRecord(value, 'Content compatibility');
  return {
    candidates: dtoArray(record.candidates, 'Content compatibility candidates').map((candidate) => {
      const entry = dtoRecord(candidate, 'Content compatibility candidate');
      return {
        loader: entry.loader === '' ? 'vanilla' : dtoEnum(entry.loader, 'Compatibility loader', LOADER_KEYS),
        loader_label: dtoString(entry.loader_label, 'Compatibility loader label'),
        game_version: dtoString(entry.game_version, 'Compatibility game version'),
        selection_id: dtoString(entry.selection_id, 'Compatibility selection'),
        summary: dtoString(entry.summary, 'Compatibility summary'),
        supported_count: dtoNumber(entry.supported_count, 'Compatibility supported count'),
        total_count: dtoNumber(entry.total_count, 'Compatibility total count'),
        complete: dtoBoolean(entry.complete, 'Compatibility complete'),
        drops: dtoArray(entry.drops, 'Compatibility drops').map((drop) => {
          const item = dtoRecord(drop, 'Compatibility drop');
          return {
            canonical_id: dtoString(item.canonical_id, 'Compatibility drop id'),
            title: dtoString(item.title, 'Compatibility drop title'),
          };
        }),
      };
    }),
    create_view: record.create_view,
  };
}

export function modpackTargetResponse(value: unknown): ModpackTarget {
  const record = dtoRecord(value, 'Modpack target');
  return {
    canonical_id: dtoString(record.canonical_id, 'Modpack id'),
    version_id: dtoString(record.version_id, 'Modpack version'),
    name: dtoString(record.name, 'Modpack name'),
    minecraft: dtoString(record.minecraft, 'Modpack Minecraft version'),
    loader: record.loader == null ? undefined : dtoEnum(record.loader, 'Modpack loader', LOADER_KEYS),
    loader_label: dtoString(record.loader_label, 'Modpack loader label'),
    selection_id: dtoString(record.selection_id, 'Modpack selection'),
  };
}

export function modpackFilesResponse(value: unknown): ModpackFilesPlan {
  const record = dtoRecord(value, 'Modpack files');
  return {
    canonical_id: dtoString(record.canonical_id, 'Modpack files id'),
    version_id: dtoString(record.version_id, 'Modpack files version'),
    name: dtoString(record.name, 'Modpack files name'),
    minecraft: dtoString(record.minecraft, 'Modpack files Minecraft version'),
    loader: record.loader == null ? null : dtoEnum(record.loader, 'Modpack files loader', LOADER_KEYS),
    files: dtoArray(record.files, 'Modpack file choices').map((file) => {
      const entry = dtoRecord(file, 'Modpack file choice');
      return {
        selection_id: dtoString(entry.selection_id, 'Modpack file selection'),
        filename: dtoString(entry.filename, 'Modpack filename'),
        kind: dtoEnum(entry.kind, 'Modpack file kind', ['mod', 'resource_pack', 'shader_pack'] as const),
        size: entry.size == null ? null : dtoNumber(entry.size, 'Modpack file size'),
        title: dtoString(entry.title, 'Modpack file title'),
        identified: dtoBoolean(entry.identified, 'Modpack file identified'),
        compatible: dtoBoolean(entry.compatible, 'Modpack file compatible'),
        installed: dtoBoolean(entry.installed, 'Modpack file installed'),
        installed_version_id: dtoOptionalString(entry.installed_version_id, 'Modpack file installed version'),
      };
    }),
  };
}

export function instanceContentResponse(value: unknown): InstanceContentResponse {
  const record = dtoRecord(value, 'Instance content');
  return {
    entries: dtoArray(record.entries, 'Instance content entries').map((content) => {
      const entry = dtoRecord(content, 'Instance content entry');
      return {
        canonical_id: dtoString(entry.canonical_id, 'Instance content id'),
        title: dtoOptionalString(entry.title, 'Instance content title'),
        kind: dtoEnum(entry.kind, 'Instance content kind', CONTENT_KINDS),
        provider: dtoEnum(entry.provider, 'Instance content provider', ['modrinth'] as const),
        project_id: dtoString(entry.project_id, 'Instance content project'),
        version_id: dtoString(entry.version_id, 'Instance content version'),
        filename: dtoString(entry.filename, 'Instance content filename'),
        enabled: dtoBoolean(entry.enabled, 'Instance content enabled'),
        ...(entry.required_by === undefined
          ? {}
          : { required_by: strings(entry.required_by, 'Instance content dependents') }),
      };
    }),
    packs: dtoArray(record.packs ?? [], 'Instance modpacks').map((value) => {
      const pack = dtoRecord(value, 'Instance modpack');
      return {
        canonical_id: dtoString(pack.canonical_id, 'Instance modpack id'),
        provider: dtoEnum(pack.provider, 'Instance modpack provider', ['modrinth', 'local'] as const),
        project_id: dtoString(pack.project_id, 'Instance modpack project'),
        title: dtoOptionalString(pack.title, 'Instance modpack title'),
        version_id: dtoString(pack.version_id, 'Instance modpack version'),
        version_number: dtoOptionalString(pack.version_number, 'Instance modpack version number'),
        paths: dtoArray(pack.paths, 'Instance modpack paths').map((path) => dtoString(path, 'Instance modpack path')),
      };
    }),
  };
}

function updatePack(value: unknown): { canonical_id: string; title?: string } {
  const pack = dtoRecord(value, 'Content update pack');
  return {
    canonical_id: dtoString(pack.canonical_id, 'Content update pack id'),
    title: dtoOptionalString(pack.title, 'Content update pack title'),
  };
}

export function contentUpdatesResponse(value: unknown): ContentUpdatesResponse {
  const record = dtoRecord(value, 'Content updates');
  return {
    updates: dtoArray(record.updates, 'Content updates list').map((update) => {
      const entry = dtoRecord(update, 'Content update');
      return {
        canonical_id: dtoString(entry.canonical_id, 'Content update id'),
        title: dtoOptionalString(entry.title, 'Content update title'),
        kind: dtoEnum(entry.kind, 'Content update kind', CONTENT_KINDS),
        current_version_id: dtoString(entry.current_version_id, 'Content current version'),
        latest_version_id: dtoString(entry.latest_version_id, 'Content latest version'),
        latest_version_number: dtoString(entry.latest_version_number, 'Content latest version number'),
        latest_channel: dtoEnum(entry.latest_channel, 'Content latest release channel', RELEASE_CHANNELS),
        ...(entry.pack === undefined ? {} : { pack: updatePack(entry.pack) }),
      };
    }),
  };
}

/** The structured refusal for disabling or removing content others need. */
export function requiredRefusal(error: unknown): RequiredRefusal | null {
  if (!isDtoRecord(error) || error.status !== 409 || !isDtoRecord(error.payload)) return null;
  const record = error.payload;
  if (record.required_by === undefined) return null;
  return {
    error: dtoString(record.error, 'Required content refusal'),
    required_by: dtoArray(record.required_by, 'Required content').map((value) => {
      const entry = dtoRecord(value, 'Required content entry');
      const title = dtoOptionalString(entry.title, 'Required content title');
      return {
        canonical_id: dtoString(entry.canonical_id, 'Required content id'),
        ...(title === undefined ? {} : { title }),
      };
    }),
  };
}
