import { modMetadata } from '../api';
import type { ContentFileMetadata } from '../../generated/ContentFileMetadata';
import type { InstanceResourcesResponse } from '../../generated/InstanceResourcesResponse';

/** A listing and how its last read went. What was read before stays while it is read again. */
export type LoadState<T> =
  | { status: 'loading'; data: T | null; error?: undefined }
  | { status: 'ready'; data: T; error?: undefined }
  | { status: 'error'; data: T | null; error: string };

export type ResourceLoadState = LoadState<InstanceResourcesResponse>;

export function emptyResources(): InstanceResourcesResponse {
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

/** Display metadata read from each mod jar, keyed by filename. */
export async function fetchModMetadata(id: string): Promise<Map<string, ContentFileMetadata>> {
  return new Map((await modMetadata(id)).map((entry) => [entry.name, entry.metadata]));
}
