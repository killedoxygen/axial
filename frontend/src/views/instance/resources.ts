import { api } from '../../api';
import type { InstanceResourceSummary } from '../../types';

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
  const res: any = await api('GET', `/instances/${encodeURIComponent(id)}/resources`);
  if (res?.error) throw new Error(res.error);
  return {
    ...emptyResources(),
    ...res,
    worlds: Array.isArray(res?.worlds) ? res.worlds : [],
    mods: Array.isArray(res?.mods) ? res.mods : [],
    screenshots: Array.isArray(res?.screenshots) ? res.screenshots : [],
    logs: Array.isArray(res?.logs) ? res.logs : [],
  };
}
