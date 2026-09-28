export type ContentKind = 'mod' | 'modpack' | 'resource_pack' | 'shader_pack';
export type ProviderId = 'modrinth';
export type ReleaseChannel = 'release' | 'beta' | 'alpha';
export type ContentSort = 'relevance' | 'downloads' | 'follows' | 'newest' | 'updated';

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

export type InstallState = 'installed';

export interface SearchHit extends CanonicalContent {
  install_state?: InstallState;
}

export interface ContentPage {
  items: SearchHit[];
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
  sha1?: string;
  sha512?: string;
  size?: number;
  dependencies: ContentDependency[];
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
  instance_id?: string;
  loader: string;
  game_version: string;
  items: PlanItem[];
  conflicts: PlanConflict[];
  total_download_bytes: number;
}

/** Where content is headed: an instance that exists, or one about to be created. */
export type TargetRef =
  | { kind: 'instance'; instance_id: string }
  | { kind: 'draft'; loader?: string; game_version: string };

export interface CompatDrop {
  canonical_id: string;
  title: string;
}

export interface CompatCandidate {
  loader: string;
  loader_label: string;
  game_version: string;
  selection_id: string;
  summary: string;
  supported_count: number;
  total_count: number;
  complete: boolean;
  drops: CompatDrop[];
}

export interface ContentCompatResponse {
  candidates: CompatCandidate[];
  create_view?: unknown;
}

export interface InstanceSetupPlanResponse {
  plan_id?: string;
  expires_at_ms: number;
  selection_id: string;
  plan: ResolutionPlan;
}

export interface ModpackTarget {
  canonical_id: string;
  version_id: string;
  name: string;
  minecraft: string;
  loader?: string;
  loader_label: string;
  selection_id: string;
}

export interface ModpackFileOption {
  selection_id: string;
  filename: string;
  kind: Exclude<ContentKind, 'modpack'>;
  size?: number | null;
  title: string;
  identified: boolean;
  compatible: boolean;
  installed: boolean;
}

export interface ModpackFilesPlan {
  canonical_id: string;
  version_id: string;
  name: string;
  minecraft: string;
  loader?: string | null;
  files: ModpackFileOption[];
}

export interface InstanceContentEntry {
  canonical_id: string;
  title?: string;
  kind: ContentKind;
  provider: ProviderId;
  project_id: string;
  version_id: string;
  filename: string;
  enabled: boolean;
}

export interface InstanceContentResponse {
  entries: InstanceContentEntry[];
}

export interface ContentUpdate {
  canonical_id: string;
  title?: string;
  kind: ContentKind;
  current_version_id: string;
  latest_version_id: string;
  latest_version_number: string;
}

export interface ContentUpdatesResponse {
  updates: ContentUpdate[];
}
