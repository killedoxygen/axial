export type LoaderBuildSubjectKind = 'loader_build';

export type LoaderComponentId =
  | 'net.fabricmc.fabric-loader'
  | 'org.quiltmc.quilt-loader'
  | 'net.minecraftforge'
  | 'net.neoforged';

export type LoaderType = 'fabric' | 'quilt' | 'forge' | 'neoforge';

export type LoaderTerm =
  | 'recommended'
  | 'latest'
  | 'snapshot'
  | 'pre_release'
  | 'release_candidate'
  | 'beta'
  | 'alpha'
  | 'nightly'
  | 'dev';

export type LoaderTermSource = 'explicit_version_label' | 'explicit_api_flag' | 'promotion_marker' | 'none';

export interface LoaderTermEvidence {
  term: LoaderTerm;
  source: LoaderTermSource;
}

export type LoaderSelectionReason =
  | 'recommended'
  | 'latest_stable'
  | 'latest'
  | 'stable'
  | 'unlabeled'
  | 'latest_unstable'
  | 'unstable'
  | 'unknown';

export type LoaderSelectionSource =
  | 'explicit_version_label'
  | 'explicit_api_flag'
  | 'promotion_marker'
  | 'absence_of_recommended'
  | 'none';

export interface LoaderSelectionMeta {
  default_rank: number;
  reason: LoaderSelectionReason;
  source: LoaderSelectionSource;
}

export interface LoaderBuildMetadata {
  terms: LoaderTerm[];
  evidence: LoaderTermEvidence[];
  selection: LoaderSelectionMeta;
  display_tags: string[];
}

export interface VersionLoaderAttachment {
  component_id: LoaderComponentId;
  component_name: string;
  build_id: string;
  loader_version: string;
  build_meta: LoaderBuildMetadata;
}

export interface LoaderBuildRecord {
  subject_kind: LoaderBuildSubjectKind;
  component_id: LoaderComponentId;
  component_name: string;
  build_id: string;
  minecraft_version: string;
  loader_version: string;
  version_id: string;
  build_meta: LoaderBuildMetadata;
  strategy: string;
  artifact_kind: string;
  installability: string;
}
