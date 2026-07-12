import { api } from './api';
import type {
  ContentCompatResponse,
  ContentDetail,
  ContentKind,
  ContentPage,
  ContentSelection,
  ContentSort,
  InstanceContentResponse,
  ModpackInstallResponse,
  ModpackTarget,
  ResolutionPlan,
  TargetRef,
} from './types-content';

export interface ContentSearchInput {
  kind: ContentKind;
  query?: string;
  loader?: string;
  gameVersion?: string;
  category?: string;
  sort?: ContentSort;
  offset?: number;
  limit?: number;
  /** Annotates each result with what this instance already has. */
  instanceId?: string;
}

export function searchContent(input: ContentSearchInput): Promise<ContentPage> {
  const params = new URLSearchParams();
  params.set('kind', input.kind);
  if (input.query) params.set('query', input.query);
  if (input.loader) params.set('loader', input.loader);
  if (input.gameVersion) params.set('game_version', input.gameVersion);
  if (input.category) params.set('category', input.category);
  if (input.sort) params.set('sort', input.sort);
  if (input.offset) params.set('offset', String(input.offset));
  if (input.limit) params.set('limit', String(input.limit));
  if (input.instanceId) params.set('instance_id', input.instanceId);
  return api<ContentPage>('GET', `/content/search?${params.toString()}`);
}

export function getContentDetail(canonicalId: string): Promise<ContentDetail> {
  return api<ContentDetail>('GET', `/content/item?id=${encodeURIComponent(canonicalId)}`);
}

export function planContent(target: TargetRef, selections: ContentSelection[]): Promise<ResolutionPlan> {
  return api<ResolutionPlan>('POST', '/content/plan', { target, selections });
}

export function installContent(instanceId: string, selections: ContentSelection[]): Promise<InstanceContentResponse> {
  return api<InstanceContentResponse>('POST', '/content/install', { instance_id: instanceId, selections });
}

/** Which instances a staged set could live in, ranked by how little each one drops. */
export function contentCompatibility(selections: ContentSelection[]): Promise<ContentCompatResponse> {
  return api<ContentCompatResponse>('POST', '/content/compatibility', { selections });
}

/** What a modpack needs, so an instance can be created for it before importing. */
export function getModpackTarget(canonicalId: string, versionId?: string): Promise<ModpackTarget> {
  const params = new URLSearchParams({ id: canonicalId });
  if (versionId) params.set('version_id', versionId);
  return api<ModpackTarget>('GET', `/content/modpack/target?${params.toString()}`);
}

export function installModpack(
  instanceId: string,
  canonicalId: string,
  versionId?: string,
): Promise<ModpackInstallResponse> {
  return api<ModpackInstallResponse>('POST', '/content/modpack/install', {
    instance_id: instanceId,
    canonical_id: canonicalId,
    version_id: versionId,
  });
}

export function listInstanceContent(instanceId: string): Promise<InstanceContentResponse> {
  return api<InstanceContentResponse>('GET', `/instances/${encodeURIComponent(instanceId)}/content`);
}

export function uninstallContent(instanceId: string, canonicalId: string): Promise<InstanceContentResponse> {
  return api<InstanceContentResponse>(
    'DELETE',
    `/instances/${encodeURIComponent(instanceId)}/content?id=${encodeURIComponent(canonicalId)}`,
  );
}
