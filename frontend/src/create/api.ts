import { request, send } from '../platform/transport';
import type { CreateInstanceRequest } from '../generated/CreateInstanceRequest';
import type { CreateInstanceResponse } from '../generated/CreateInstanceResponse';
import type { CreateLoaderBuildsView } from '../generated/CreateLoaderBuildsView';
import type { CreateView } from '../generated/CreateView';
import type { ModpackTarget } from '../generated/ModpackTarget';

export function createView(source: string): Promise<CreateView> {
  return request('GET', `/instances/create-view?source=${encodeURIComponent(source)}`);
}

export function loaderBuilds(source: string, minecraftVersion: string): Promise<CreateLoaderBuildsView> {
  const query = new URLSearchParams({ source, minecraft_version: minecraftVersion });
  return request('GET', `/instances/create-view/loader-builds?${query}`);
}

export function createInstance(body: CreateInstanceRequest): Promise<CreateInstanceResponse> {
  return request('POST', '/instances', body);
}

/** Creates the instance a setup plan described, then installs its content. */
export function executeSetup(planId: string, body: CreateInstanceRequest): Promise<CreateInstanceResponse> {
  return request('POST', '/instances/setup', { plan_id: planId, ...body });
}

export function createFromModpack(
  canonicalId: string,
  versionId: string,
  body: CreateInstanceRequest,
): Promise<CreateInstanceResponse> {
  return request('POST', '/instances/modpack', { canonical_id: canonicalId, version_id: versionId, ...body });
}

/** Identifies a local `.mrpack` so it can be set up through the ordinary modpack flow. */
export function identifyModpackFile(file: Blob): Promise<ModpackTarget> {
  return send('POST', '/content/modpack/file', file, 'application/octet-stream');
}
