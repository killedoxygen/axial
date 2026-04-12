import { api, API } from '../api';
import type {
  LoaderBuildRecord,
  LoaderBuildsResponse,
  LoaderCatalogState,
  LoaderComponentId,
  LoaderComponentRecord,
  LoaderComponentsResponse,
} from './types';

let componentsCache: LoaderComponentRecord[] | null = null;
let buildsCache: Record<string, LoaderBuildRecord[]> = {};

export async function fetchLoaderComponents(): Promise<LoaderComponentRecord[]> {
  if (componentsCache) return componentsCache;

  const res = await api('GET', '/loaders/components') as LoaderComponentsResponse & { error?: string };
  if (res.error) throw new Error(res.error);
  componentsCache = res.components || [];
  return componentsCache;
}

export async function fetchLoaderBuilds(
  componentId: LoaderComponentId,
  minecraftVersion: string,
): Promise<LoaderBuildRecord[]> {
  const key = `${componentId}:${minecraftVersion}`;
  if (buildsCache[key]) return buildsCache[key];

  const res = await api(
    'GET',
    `/loaders/components/${encodeURIComponent(componentId)}/builds?mc_version=${encodeURIComponent(minecraftVersion)}`,
  ) as LoaderBuildsResponse & { error?: string };
  if (res.error) throw new Error(res.error);
  buildsCache[key] = res.builds || [];
  return buildsCache[key];
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
  buildsCache = {};
}
