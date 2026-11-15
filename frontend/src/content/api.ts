import { isApiError, request } from '../platform/transport';
import { reconcileUncertainMutation } from '../install/queue';
import type { Page } from '../generated/Page';
import type { SearchItem } from '../generated/SearchItem';
import type { CompatibilityResponse } from '../generated/CompatibilityResponse';
import type { ContentDetail } from '../generated/ContentDetail';
import type { ContentKind } from '../generated/ContentKind';
import type { ResolutionSelection } from '../generated/ResolutionSelection';
import type { UpdatesResponse } from '../generated/UpdatesResponse';
import type { InstalledContent } from '../generated/InstalledContent';
import type { InstanceSetupPlanResponse } from '../generated/InstanceSetupPlanResponse';
import type { ModpackTarget } from '../generated/ModpackTarget';
import type { ModpackFilesPlan } from '../generated/ModpackFilesPlan';
import type { ResolutionPlan } from '../generated/ResolutionPlan';
import type { TargetRef } from '../generated/TargetRef';
import type { RequiredRefusal } from '../generated/RequiredRefusal';
import type { InstallQueueStateResponse } from '../generated/InstallQueueStateResponse';

export type ContentSort = 'relevance' | 'downloads' | 'follows' | 'newest' | 'updated';
export type ContentPage = Page<SearchItem>;

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

async function queueResponse(command: Promise<InstallQueueStateResponse>): Promise<InstallQueueStateResponse> {
  try {
    return await command;
  } catch (error) {
    await reconcileUncertainMutation();
    throw error;
  }
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
  return request<ContentPage>('GET', `/content/search?${params.toString()}`);
}

export function getContentDetail(canonicalId: string): Promise<ContentDetail> {
  return request('GET', `/content/item?id=${encodeURIComponent(canonicalId)}`);
}

export function planContent(target: TargetRef, selections: ResolutionSelection[]): Promise<ResolutionPlan> {
  return request('POST', '/content/plan', { target, selections });
}

export function planInstanceSetup(
  selectionId: string,
  target: TargetRef,
  selections: ResolutionSelection[],
): Promise<InstanceSetupPlanResponse> {
  return request('POST', '/instances/setup/plan', {
    selection_id: selectionId,
    target,
    selections,
  });
}

/** `label` names the queue entry for display; recorded content is labelled by the backend. */
export function installContent(
  instanceId: string,
  selections: ResolutionSelection[],
  allowIncompatible = false,
  replaceFiles: string[] = [],
  label?: string,
): Promise<InstallQueueStateResponse> {
  return queueResponse(
    request('POST', '/content/install', {
      instance_id: instanceId,
      selections,
      allow_incompatible: allowIncompatible,
      ...(replaceFiles.length > 0 ? { replace_files: replaceFiles } : {}),
      ...(label ? { label } : {}),
    }),
  );
}

/** Which instances a staged set could live in, ranked by how little each one drops. */
export function contentCompatibility(selections: ResolutionSelection[]): Promise<CompatibilityResponse> {
  return request('POST', '/content/compatibility', { selections });
}

/** Narrows the default pack version when no exact version is chosen. */
export interface ModpackFilter {
  gameVersion?: string;
  loader?: string;
}

/** What a modpack needs, so an instance can be created for it before importing. */
export function getModpackTarget(
  canonicalId: string,
  versionId?: string,
  filter: ModpackFilter = {},
): Promise<ModpackTarget> {
  const params = new URLSearchParams({ id: canonicalId });
  if (versionId) params.set('version_id', versionId);
  else {
    if (filter.gameVersion) params.set('game_version', filter.gameVersion);
    if (filter.loader) params.set('loader', filter.loader);
  }
  return request('GET', `/content/modpack/target?${params.toString()}`);
}

export function getModpackFiles(
  instanceId: string,
  canonicalId: string,
  versionId?: string,
): Promise<ModpackFilesPlan> {
  const params = new URLSearchParams({ instance_id: instanceId, id: canonicalId });
  if (versionId) params.set('version_id', versionId);
  return request('GET', `/content/modpack/files?${params.toString()}`);
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
  return queueResponse(
    request('POST', '/content/modpack/install', {
      instance_id: instanceId,
      canonical_id: canonicalId,
      version_id: versionId,
      selected_file_ids: options.selectedFileIds ?? [],
      include_overrides: options.includeOverrides ?? true,
    }),
  );
}

export function listInstanceContent(instanceId: string): Promise<InstalledContent> {
  return request('GET', `/instances/${encodeURIComponent(instanceId)}/content`);
}

export function checkContentUpdates(instanceId: string): Promise<UpdatesResponse> {
  return request('GET', `/instances/${encodeURIComponent(instanceId)}/content/updates`);
}

export function uninstallContents(instanceId: string, canonicalIds: string[]): Promise<InstallQueueStateResponse> {
  return queueResponse(
    request('POST', `/instances/${encodeURIComponent(instanceId)}/content/uninstall`, {
      canonical_ids: canonicalIds,
    }),
  );
}

/** The structured refusal for disabling or removing content others need. */
export function requiredRefusal(error: unknown): RequiredRefusal | null {
  if (!isApiError(error) || error.status !== 409 || typeof error.payload !== 'object' || error.payload === null) {
    return null;
  }
  return 'required_by' in error.payload ? (error.payload as RequiredRefusal) : null;
}
