import { api, API } from '../api';
import type {
  LoaderBuildRecord,
  LoaderBuildsResponse,
  LoaderCatalogState,
  LoaderComponentId,
  LoaderComponentRecord,
  LoaderComponentsResponse,
  LoaderGameVersion,
  LoaderGameVersionsResponse,
} from './types';

let componentsCache: LoaderComponentRecord[] | null = null;
let componentsPromise: Promise<LoaderComponentRecord[]> | null = null;
let supportedVersionsCache: Record<string, LoaderGameVersion[]> = {};
let supportedVersionsPromiseCache: Partial<Record<string, Promise<LoaderGameVersion[]>>> = {};
let buildsCache: Record<string, LoaderBuildRecord[]> = {};
let buildsPromiseCache: Partial<Record<string, Promise<LoaderBuildRecord[]>>> = {};

export function getCachedLoaderComponents(): LoaderComponentRecord[] | null {
  return componentsCache;
}

export function getCachedLoaderSupportedVersions(
  componentId: LoaderComponentId,
): LoaderGameVersion[] | null {
  return supportedVersionsCache[componentId] ?? null;
}

export function getCachedLoaderBuilds(
  componentId: LoaderComponentId,
  minecraftVersion: string,
): LoaderBuildRecord[] | null {
  return buildsCache[`${componentId}:${minecraftVersion}`] ?? null;
}

export async function fetchLoaderComponents(): Promise<LoaderComponentRecord[]> {
  if (componentsCache) return componentsCache;
  if (componentsPromise) return componentsPromise;

  componentsPromise = (async () => {
    const res = await api('GET', '/loaders/components') as LoaderComponentsResponse & { error?: string };
    if (res.error) throw new Error(res.error);
    componentsCache = res.components || [];
    return componentsCache;
  })();

  try {
    return await componentsPromise;
  } finally {
    componentsPromise = null;
  }
}

export async function fetchLoaderBuilds(
  componentId: LoaderComponentId,
  minecraftVersion: string,
): Promise<LoaderBuildRecord[]> {
  const key = `${componentId}:${minecraftVersion}`;
  if (buildsCache[key]) return buildsCache[key];
  if (buildsPromiseCache[key]) return buildsPromiseCache[key];

  buildsPromiseCache[key] = (async () => {
    const res = await api(
      'GET',
      `/loaders/components/${encodeURIComponent(componentId)}/builds?mc_version=${encodeURIComponent(minecraftVersion)}`,
    ) as LoaderBuildsResponse & { error?: string };
    if (res.error) throw new Error(res.error);
    buildsCache[key] = res.builds || [];
    return buildsCache[key];
  })();

  try {
    return await buildsPromiseCache[key];
  } finally {
    delete buildsPromiseCache[key];
  }
}

export async function fetchLoaderSupportedVersions(
  componentId: LoaderComponentId,
): Promise<LoaderGameVersion[]> {
  if (supportedVersionsCache[componentId]) return supportedVersionsCache[componentId];
  if (supportedVersionsPromiseCache[componentId]) return supportedVersionsPromiseCache[componentId];

  supportedVersionsPromiseCache[componentId] = (async () => {
    const res = await api(
      'GET',
      `/loaders/components/${encodeURIComponent(componentId)}/game-versions`,
    ) as LoaderGameVersionsResponse & { error?: string };
    if (res.error) throw new Error(res.error);
    supportedVersionsCache[componentId] = res.versions || [];
    return supportedVersionsCache[componentId];
  })();

  try {
    return await supportedVersionsPromiseCache[componentId];
  } finally {
    delete supportedVersionsPromiseCache[componentId];
  }
}

export async function startLoaderInstall(
  componentId: LoaderComponentId,
  buildId: string,
): Promise<string> {
  const res: any = await api('POST', '/loaders/install', {
    component_id: componentId,
    build_id: buildId,
  });
  if (res.error) throw new Error(res.error);
  return res.install_id;
}

export function connectLoaderInstallSSE(
  installId: string,
  onProgress: (data: any) => void,
  onDone: () => void,
  onError: (message: string) => void,
): EventSource {
  const es = new EventSource(`${API}/loaders/install/${installId}/events`);

  es.addEventListener('progress', (e: MessageEvent) => {
    const data = JSON.parse(e.data);
    if (data.phase === 'error' || data.error) {
      onError(data.error || 'Unknown error');
      es.close();
      return;
    }
    onProgress(data);
    if (data.done) {
      onDone();
      es.close();
    }
  });

  es.onerror = (): void => {
    if (es.readyState !== EventSource.CLOSED) return;
    onError('Connection lost');
  };

  return es;
}

export function clearLoaderCaches(): void {
  componentsCache = null;
  componentsPromise = null;
  supportedVersionsCache = {};
  supportedVersionsPromiseCache = {};
  buildsCache = {};
  buildsPromiseCache = {};
}
