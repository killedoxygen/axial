import { api, API } from './api';
import type { LoaderType, GameVersion, LoaderVersion, LoaderInfo, CatalogVersion } from './types';

// In-memory caches (reset on page reload)
let gameVersionsCache: Record<string, GameVersion[]> = {};   // { fabric: [{version, stable}] }
let loaderVersionsCache: Record<string, LoaderVersion[]> = {}; // { "fabric:1.21.4": [{version, stable}] }

export const LOADER_TYPES: LoaderInfo[] = [
  { type: 'fabric',   name: 'Fabric' },
  { type: 'quilt',    name: 'Quilt' },
  { type: 'forge',    name: 'Forge' },
  { type: 'neoforge', name: 'NeoForge' },
];

// Fetch game versions supported by a loader (cached)
export async function fetchGameVersions(loaderType: string): Promise<GameVersion[]> {
  if (gameVersionsCache[loaderType]) return gameVersionsCache[loaderType];

  try {
    const res: any = await api('GET', `/loaders/${loaderType}/game-versions`);
    if (res.error) throw new Error(res.error);
    const versions: GameVersion[] = res.game_versions || [];
    gameVersionsCache[loaderType] = versions;
    return versions;
  } catch (err: unknown) {
    // Return cached even if stale
    if (gameVersionsCache[loaderType]) return gameVersionsCache[loaderType];
    throw err;
  }
}

// Fetch loader versions for a specific game version (cached)
export async function fetchLoaderVersions(loaderType: string, mcVersion: string): Promise<LoaderVersion[]> {
  const key: string = `${loaderType}:${mcVersion}`;
  if (loaderVersionsCache[key]) return loaderVersionsCache[key];

  try {
    const res: any = await api('GET', `/loaders/${loaderType}/loader-versions?mc_version=${encodeURIComponent(mcVersion)}`);
    if (res.error) throw new Error(res.error);
    const versions: LoaderVersion[] = res.loader_versions || [];
    loaderVersionsCache[key] = versions;
    return versions;
  } catch (err: unknown) {
    if (loaderVersionsCache[key]) return loaderVersionsCache[key];
    throw err;
  }
}

// Filter catalog versions to those supported by the loader
export function filterByLoaderSupport(catalogVersions: CatalogVersion[], loaderGameVersions: GameVersion[]): CatalogVersion[] {
  const supported: Set<string> = new Set(loaderGameVersions.map((v: GameVersion) => v.version));
  return catalogVersions.filter((v: CatalogVersion) => supported.has(v.id));
}

// Get the latest stable loader version (or first if none stable)
export function latestStable(loaderVersions: LoaderVersion[]): LoaderVersion | null {
  const stable: LoaderVersion | undefined = loaderVersions.find((v: LoaderVersion) => v.stable || v.recommended);
  return stable || loaderVersions[0] || null;
}

// Start a loader install, returns install_id
export async function startLoaderInstall(loaderType: string, gameVersion: string, loaderVersion: string): Promise<string> {
  const res: any = await api('POST', '/loaders/install', {
    loader_type: loaderType,
    game_version: gameVersion,
    loader_version: loaderVersion,
  });
  if (res.error) throw new Error(res.error);
  return res.install_id;
}

// Connect to loader install SSE stream
export function connectLoaderInstallSSE(
  installId: string,
  onProgress: (data: any) => void,
  onDone: () => void,
  onError: (msg: string) => void,
): EventSource {
  const es: EventSource = new EventSource(`${API}/loaders/install/${installId}/events`);

  es.addEventListener('progress', (e: MessageEvent) => {
    const d: any = JSON.parse(e.data);
    if (d.phase === 'error' || d.error) {
      onError(d.error || 'Unknown error');
      es.close();
      return;
    }
    onProgress(d);
    if (d.done) {
      onDone();
      es.close();
    }
  });

  es.onerror = (): void => {
    onError('Connection lost');
    es.close();
  };

  return es;
}

export function clearCaches(): void {
  gameVersionsCache = {};
  loaderVersionsCache = {};
}
