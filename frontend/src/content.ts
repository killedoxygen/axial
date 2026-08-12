import { api } from './api';
import {
  contentCompatResponse,
  contentDetailResponse,
  contentPageResponse,
  contentUpdatesResponse,
  instanceContentResponse,
  instanceSetupPlanResponse,
  modpackFilesResponse,
  modpackTargetResponse,
  resolutionPlanResponse,
} from './dto-content';
import { installQueueStateResponse } from './dto-install';
import type {
  ContentCompatResponse,
  ContentDetail,
  ContentKind,
  ContentPage,
  ContentSelection,
  ContentUpdatesResponse,
  ContentSort,
  InstanceContentResponse,
  InstanceSetupPlanResponse,
  ModpackTarget,
  ModpackFilesPlan,
  ResolutionPlan,
  TargetRef,
} from './types-content';
import type { InstallQueueStateResponse } from './types-install';

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
  return api('GET', `/content/search?${params.toString()}`).then(contentPageResponse);
}

export function getContentDetail(canonicalId: string): Promise<ContentDetail> {
  return api('GET', `/content/item?id=${encodeURIComponent(canonicalId)}`).then(contentDetailResponse);
}

export function planContent(target: TargetRef, selections: ContentSelection[]): Promise<ResolutionPlan> {
  return api('POST', '/content/plan', { target, selections }).then(resolutionPlanResponse);
}

export function planInstanceSetup(
  selectionId: string,
  target: TargetRef,
  selections: ContentSelection[],
): Promise<InstanceSetupPlanResponse> {
  return api('POST', '/instances/setup/plan', {
    selection_id: selectionId,
    target,
    selections,
  }).then(instanceSetupPlanResponse);
}

export function installContent(
  instanceId: string,
  selections: ContentSelection[],
  allowIncompatible = false,
): Promise<InstallQueueStateResponse> {
  return api('POST', '/content/install', {
    instance_id: instanceId,
    selections,
    allow_incompatible: allowIncompatible,
  }).then(installQueueStateResponse);
}

/** Which instances a staged set could live in, ranked by how little each one drops. */
export function contentCompatibility(selections: ContentSelection[]): Promise<ContentCompatResponse> {
  return api('POST', '/content/compatibility', { selections }).then(contentCompatResponse);
}

/** What a modpack needs, so an instance can be created for it before importing. */
export function getModpackTarget(canonicalId: string, versionId?: string): Promise<ModpackTarget> {
  const params = new URLSearchParams({ id: canonicalId });
  if (versionId) params.set('version_id', versionId);
  return api('GET', `/content/modpack/target?${params.toString()}`).then(modpackTargetResponse);
}

export function getModpackFiles(
  instanceId: string,
  canonicalId: string,
  versionId?: string,
): Promise<ModpackFilesPlan> {
  const params = new URLSearchParams({ instance_id: instanceId, id: canonicalId });
  if (versionId) params.set('version_id', versionId);
  return api('GET', `/content/modpack/files?${params.toString()}`).then(modpackFilesResponse);
}

export function installModpack(
  instanceId: string,
  canonicalId: string,
  versionId?: string,
  options: {
    selectedFileIds?: string[];
    includeOverrides?: boolean;
  } = {},
): Promise<InstallQueueStateResponse> {
  return api('POST', '/content/modpack/install', {
    instance_id: instanceId,
    canonical_id: canonicalId,
    version_id: versionId,
    selected_file_ids: options.selectedFileIds ?? [],
    include_overrides: options.includeOverrides ?? true,
  }).then(installQueueStateResponse);
}

export function listInstanceContent(instanceId: string): Promise<InstanceContentResponse> {
  return api('GET', `/instances/${encodeURIComponent(instanceId)}/content`).then(instanceContentResponse);
}

export function checkContentUpdates(instanceId: string): Promise<ContentUpdatesResponse> {
  return api('GET', `/instances/${encodeURIComponent(instanceId)}/content/updates`).then(contentUpdatesResponse);
}

export function uninstallContents(instanceId: string, canonicalIds: string[]): Promise<InstallQueueStateResponse> {
  return api('POST', `/instances/${encodeURIComponent(instanceId)}/content/uninstall`, {
    canonical_ids: canonicalIds,
  }).then(installQueueStateResponse);
}
