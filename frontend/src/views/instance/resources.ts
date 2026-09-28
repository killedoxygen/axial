import { api } from '../../api';
import type { InstanceResourceSummary } from '../../types-instance';
import { instanceResourcesResponse } from '../../dto-core';
import { dtoWithoutError, type DtoRecord } from '../../dto-contract';

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

export function requireResourceCommandSuccess(value: unknown, label: string): DtoRecord {
  const response = dtoWithoutError(value, label);
  if (response.status !== 'ok') throw new Error(`${label} was not confirmed. Refresh the list before trying again.`);
  return response;
}
