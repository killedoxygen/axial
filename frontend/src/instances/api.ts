import { apiResourceUrl, request, send } from '../platform/transport';
import type { DeletionResponse } from '../generated/DeletionResponse';
import type { DeletionSnapshot } from '../generated/DeletionSnapshot';
import type { EnrichedInstance } from '../generated/EnrichedInstance';
import type { InstanceLogTailResponse } from '../generated/InstanceLogTailResponse';
import type { InstanceModMetadata } from '../generated/InstanceModMetadata';
import type { InstancePackInfo } from '../generated/InstancePackInfo';
import type { InstancePatch } from '../generated/InstancePatch';
import type { InstanceResourcesResponse } from '../generated/InstanceResourcesResponse';
import type { JavaRuntimesResponse } from '../generated/JavaRuntimesResponse';
import type { ModImportResponse } from '../generated/ModImportResponse';
import type { ListResponse } from '../generated/ListResponse';
import type { CreateInstanceResponse } from '../generated/CreateInstanceResponse';
import type { PendingInstancesResponse } from '../generated/PendingInstancesResponse';
import type { PerformanceHealthResponse } from '../generated/PerformanceHealthResponse';
import type { ResourceCommand } from '../generated/ResourceCommand';
import type { StatusResponse } from '../generated/StatusResponse';
import type { VersionsResponse } from '../generated/VersionsResponse';

const instance = (id: string): string => `/instances/${encodeURIComponent(id)}`;
const named = (id: string, folder: string, name: string): string =>
  `${instance(id)}/${folder}/${encodeURIComponent(name)}`;

export function listInstances(): Promise<ListResponse> {
  return request('GET', '/instances');
}

export function getInstance(id: string): Promise<EnrichedInstance> {
  return request('GET', instance(id));
}

export function updateInstance(id: string, patch: InstancePatch): Promise<EnrichedInstance> {
  return request('PUT', instance(id), patch);
}

export function duplicateInstance(id: string): Promise<EnrichedInstance> {
  return request('POST', `${instance(id)}/duplicate`, {});
}

export function resumeSetup(id: string): Promise<CreateInstanceResponse> {
  return request('POST', `${instance(id)}/setup/resume`, {});
}

export function deleteInstance(id: string, keepFiles: boolean, operationId: string): Promise<DeletionResponse> {
  const query = new URLSearchParams({ operation_id: operationId, keep_files: String(keepFiles) });
  return request('DELETE', `${instance(id)}?${query}`);
}

export function deletionStatus(operationId: string): Promise<DeletionSnapshot> {
  return request('GET', `/instances/deletions/${encodeURIComponent(operationId)}`);
}

export function pendingOperations(): Promise<PendingInstancesResponse> {
  return request('GET', '/instances/pending');
}

export function openFolder(id: string, sub?: string): Promise<StatusResponse> {
  const query = sub ? `?${new URLSearchParams({ sub })}` : '';
  return request('POST', `${instance(id)}/open-folder${query}`);
}

export function listVersions(): Promise<VersionsResponse> {
  return request('GET', '/versions');
}

export function listJavaRuntimes(): Promise<JavaRuntimesResponse> {
  return request('GET', '/java');
}

export function performanceHealth(id: string): Promise<PerformanceHealthResponse> {
  return request('GET', `/performance/health?${new URLSearchParams({ instance_id: id })}`);
}

export function resources(id: string): Promise<InstanceResourcesResponse> {
  return request('GET', `${instance(id)}/resources`);
}

export function modMetadata(id: string): Promise<InstanceModMetadata[]> {
  return request('GET', `${instance(id)}/mods/metadata`);
}

/** The instance folders whose packs the game selects in its own menus. */
export type PackFolder = 'resourcepacks' | 'shaderpacks';

export function packs(id: string, folder: PackFolder): Promise<InstancePackInfo[]> {
  return request('GET', `${instance(id)}/${folder}`);
}

export function deletePack(id: string, folder: PackFolder, name: string): Promise<ResourceCommand> {
  return request('DELETE', named(id, folder, name));
}

export function setModEnabled(
  id: string,
  name: string,
  enabled: boolean,
  withDependents: boolean,
): Promise<ResourceCommand> {
  return request('PUT', named(id, 'mods', name), withDependents ? { enabled, with_dependents: true } : { enabled });
}

/** One jar per request, so a refusal names its own file. */
export function importMod(id: string, file: File): Promise<ModImportResponse> {
  return send(
    'POST',
    `${instance(id)}/mods/import?${new URLSearchParams({ name: file.name })}`,
    file,
    'application/octet-stream',
  );
}

export function deleteMod(id: string, name: string): Promise<ResourceCommand> {
  return request('DELETE', named(id, 'mods', name));
}

export function renameWorld(id: string, name: string, next: string): Promise<ResourceCommand> {
  return request('PUT', named(id, 'worlds', name), { name: next });
}

export function deleteWorld(id: string, name: string): Promise<ResourceCommand> {
  return request('DELETE', named(id, 'worlds', name));
}

export function backupWorld(id: string, name: string): Promise<ResourceCommand> {
  return request('POST', `${named(id, 'worlds', name)}/backup`, {});
}

export function renameScreenshot(id: string, name: string, next: string): Promise<ResourceCommand> {
  return request('PUT', named(id, 'screenshots', name), { name: next });
}

export function deleteScreenshot(id: string, name: string): Promise<ResourceCommand> {
  return request('DELETE', named(id, 'screenshots', name));
}

export function screenshotUrl(id: string, name: string): string {
  return apiResourceUrl(`${named(id, 'screenshots', name)}/file`);
}

export function logTail(id: string, name: string): Promise<InstanceLogTailResponse> {
  return request('GET', named(id, 'logs', name));
}

/** The folders whose files carry their own art. */
export type ArtFolder = 'mods' | 'resourcepacks' | 'worlds';

/** A file's own art, addressed by its revision so it is fetched once per change. */
export function fileArtUrl(id: string, folder: ArtFolder, name: string, revision: string): string {
  return apiResourceUrl(`${named(id, folder, name)}/icon?v=${encodeURIComponent(revision)}`);
}
