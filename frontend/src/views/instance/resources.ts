import { api, apiResourceUrl } from '../../api';
import type { InstanceResourceSummary } from '../../types-instance';
import { instanceResourcesResponse } from '../../dto-core';
import {
  dtoArray,
  dtoBoolean,
  dtoOptionalNumber,
  dtoOptionalString,
  dtoRecord,
  dtoString,
  dtoWithoutError,
  type DtoRecord,
} from '../../dto-contract';
import type { ContentFileMetadata } from '../../generated/ContentFileMetadata';
import type { InstanceModMetadata } from '../../generated/InstanceModMetadata';
import type { InstancePackInfo } from '../../generated/InstancePackInfo';

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

/** The instance folders whose packs the game selects in its own menus. */
export type PackFolder = 'resourcepacks' | 'shaderpacks';

/** Display metadata read from each mod jar, keyed by filename. */
export async function fetchModMetadata(id: string): Promise<Map<string, ContentFileMetadata>> {
  const entries = instanceModMetadataResponse(await api('GET', `/instances/${encodeURIComponent(id)}/mods/metadata`));
  return new Map(entries.map((entry) => [entry.name, entry.metadata]));
}

export async function fetchPacks(id: string, folder: PackFolder): Promise<InstancePackInfo[]> {
  return instancePacksResponse(await api('GET', `/instances/${encodeURIComponent(id)}/${folder}`));
}

export function modIconUrl(instanceId: string, name: string): string {
  return apiResourceUrl(`/instances/${encodeURIComponent(instanceId)}/mods/${encodeURIComponent(name)}/icon`);
}

export function resourcePackIconUrl(instanceId: string, name: string): string {
  return apiResourceUrl(`/instances/${encodeURIComponent(instanceId)}/resourcepacks/${encodeURIComponent(name)}/icon`);
}

function contentFileMetadataResponse(value: unknown, label: string): ContentFileMetadata {
  const record = dtoRecord(value, label);
  return {
    title: dtoOptionalString(record.title, `${label} title`),
    version: dtoOptionalString(record.version, `${label} version`),
    authors: dtoArray(record.authors, `${label} authors`).map((author) => dtoString(author, `${label} author`)),
    description: dtoOptionalString(record.description, `${label} description`),
    icon: dtoBoolean(record.icon, `${label} icon`),
  };
}

export function instanceModMetadataResponse(value: unknown): InstanceModMetadata[] {
  return dtoArray(value, 'Mod metadata').map((item) => {
    const entry = dtoRecord(item, 'Mod metadata entry');
    return {
      name: dtoString(entry.name, 'Mod metadata name'),
      metadata: contentFileMetadataResponse(entry.metadata, 'Mod metadata'),
    };
  });
}

export function instancePacksResponse(value: unknown): InstancePackInfo[] {
  return dtoArray(value, 'Packs').map((item) => {
    const entry = dtoRecord(item, 'Pack');
    return {
      name: dtoString(entry.name, 'Pack name'),
      directory: dtoBoolean(entry.directory, 'Pack folder'),
      size: dtoOptionalNumber(entry.size, 'Pack size'),
      modified_at: dtoString(entry.modified_at, 'Pack modification time'),
      enabled: dtoBoolean(entry.enabled, 'Pack enabled'),
      metadata: contentFileMetadataResponse(entry.metadata, 'Pack metadata'),
    };
  });
}
