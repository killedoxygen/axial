import { api } from './api';
import type {
  ContentDetail,
  ContentKind,
  ContentPage,
  ContentSelection,
  ContentSort,
  InstanceContentResponse,
  ResolutionPlan,
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
  return api<ContentPage>('GET', `/content/search?${params.toString()}`);
}

export function getContentDetail(canonicalId: string): Promise<ContentDetail> {
  return api<ContentDetail>('GET', `/content/item?id=${encodeURIComponent(canonicalId)}`);
}

export function planContent(instanceId: string, selections: ContentSelection[]): Promise<ResolutionPlan> {
  return api<ResolutionPlan>('POST', '/content/plan', { instance_id: instanceId, selections });
}

export function installContent(instanceId: string, selections: ContentSelection[]): Promise<InstanceContentResponse> {
  return api<InstanceContentResponse>('POST', '/content/install', { instance_id: instanceId, selections });
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
