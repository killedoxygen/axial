import type { VersionLoaderAttachment } from './types-loader';

export type LifecycleChannel = 'stable' | 'preview' | 'experimental' | 'legacy' | 'unknown';

export type LifecycleLabel =
  | 'release'
  | 'recommended'
  | 'latest'
  | 'snapshot'
  | 'pre_release'
  | 'release_candidate'
  | 'beta'
  | 'alpha'
  | 'old_beta'
  | 'old_alpha'
  | 'nightly'
  | 'dev'
  | 'unknown';

export interface LifecycleMeta {
  channel: LifecycleChannel;
  labels: LifecycleLabel[];
  default_rank: number;
  badge_text: string;
  provider_terms: string[];
}

export interface MinecraftVersionMeta {
  family: string;
  base_id: string;
  effective_version: string;
  variant_of: string;
  variant_kind: string;
  display_name: string;
  display_hint: string;
}

export type VersionSubjectKind = 'installed_version' | 'minecraft_version';

export interface Version {
  subject_kind: VersionSubjectKind;
  id: string;
  raw_kind: string;
  release_time?: string;
  minecraft_meta: MinecraftVersionMeta;
  lifecycle: LifecycleMeta;
  inherits_from?: string;
  launchable: boolean;
  installed: boolean;
  status: string;
  status_detail?: string;
  needs_install?: string;
  java_component?: string;
  java_major?: number;
  loader?: VersionLoaderAttachment | null;
}

export interface CatalogVersion {
  subject_kind: VersionSubjectKind;
  id: string;
  raw_kind: string;
  release_time: string;
  minecraft_meta: MinecraftVersionMeta;
  lifecycle: LifecycleMeta;
  url: string;
  installed: boolean;
}

export interface Catalog {
  latest: { release: string; snapshot: string };
  versions: CatalogVersion[];
}
