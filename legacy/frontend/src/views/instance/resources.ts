import { api } from '../../api';
import type { InstanceResourceSummary } from '../../types-instance';
import { instanceResourcesResponse } from '../../dto-core';

export type ResourceLoadState =
  | { status: 'loading'; data: InstanceResourceSummary | null; error?: undefined }
  | { status: 'ready'; data: InstanceResourceSummary; error?: undefined }
  | { status: 'error'; data: InstanceResourceSummary | null; error: string };

export function emptyResources(): InstanceResourceSummary {
  return {
    worlds: [],
    mods: [],
    screenshots: [],
    logs: [],
    worlds_count: 0,
    mods_count: 0,
    screenshots_count: 0,
    logs_count: 0,
  };
}

export async function fetchInstanceResources(id: string): Promise<InstanceResourceSummary> {
  return instanceResourcesResponse(await api('GET', `/instances/${encodeURIComponent(id)}/resources`));
}
