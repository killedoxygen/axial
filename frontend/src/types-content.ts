export type ContentKind = 'mod' | 'modpack' | 'resource_pack' | 'shader_pack';
export type ProviderId = 'modrinth';
export type ReleaseChannel = 'release' | 'beta' | 'alpha';
export type ContentSort = 'relevance' | 'downloads' | 'follows' | 'newest' | 'updated';

export interface ProviderRef {
  provider: ProviderId;
  project_id: string;
  slug?: string;
}

export interface CanonicalContent {
  canonical_id: string;
  kind: ContentKind;
  provider: ProviderId;
  project_id: string;
  slug?: string;
  title: string;
  author: string;
  summary: string;
  icon_url?: string;
  downloads: number;
  follows: number;
  categories: string[];
  game_versions: string[];
  loaders: string[];
  updated?: string;
  sources: ProviderRef[];
}

export interface FileRef {
  url: string;
  filename: string;
  sha1?: string;
  sha512?: string;
  size?: number;
  primary: boolean;
}

export type DependencyKind = 'required' | 'optional' | 'incompatible' | 'embedded';

export interface ContentDependency {
  project_id?: string;
  version_id?: string;
  kind: DependencyKind;
}

export interface ContentVersion {
  id: string;
  name: string;
  version_number: string;
  game_versions: string[];
  loaders: string[];
  channel: ReleaseChannel;
  published?: string;
  downloads: number;
  files: FileRef[];
  dependencies: ContentDependency[];
}

export interface GalleryImage {
  url: string;
  title?: string;
}

export interface ContentDetail extends CanonicalContent {
  body: string;
  gallery: GalleryImage[];
  versions: ContentVersion[];
}

export interface ContentPage {
  items: CanonicalContent[];
  offset: number;
  limit: number;
  total: number;
}

export interface ContentSelection {
  canonical_id: string;
  kind: ContentKind;
  version_id?: string;
}

export type PlanReason = 'selected' | 'dependency';

export interface PlanItem {
  canonical_id: string;
  title: string;
  kind: ContentKind;
  project_id: string;
  version_id: string;
  version_number: string;
  filename: string;
  size?: number;
  reason: PlanReason;
  already_installed: boolean;
  update: boolean;
}

export type ConflictKind = 'unavailable' | 'incompatible';

export interface PlanConflict {
  canonical_id?: string;
  kind: ConflictKind;
  detail: string;
}

export interface ResolutionPlan {
  instance_id: string;
  loader: string;
  game_version: string;
  items: PlanItem[];
  conflicts: PlanConflict[];
  total_download_bytes: number;
}

export type EntrySource = 'managed' | 'imported';

export interface InstanceContentEntry {
  canonical_id: string;
  title?: string;
  kind: ContentKind;
  provider: ProviderId;
  project_id: string;
  version_id: string;
  filename: string;
  enabled: boolean;
  source: EntrySource;
}

export interface InstanceContentResponse {
  entries: InstanceContentEntry[];
}
