import { api, API } from './api';
import { showError, errMessage } from './utils';
import { startLoaderInstall, connectLoaderInstallSSE } from './loaders/api';
import { createProgressEstimator } from './progress-estimation';
import {
  hasNativeDesktopRuntime, nativeInstallEventName, nativeLoaderInstallEventName,
  onNativeEvent, startNativeInstallEvents, startNativeLoaderInstallEvents,
} from './native';
import {
  selectedInstance, selectedVersion, installState, catalog, versions,
} from './store';
import {
  enqueueInstall, dequeueNextInstall, startInstall, updateInstallProgress,
  completeInstall, setInstallEventSource,
} from './actions';
import type { InstallItem, LoaderBuildRecord, LoaderComponentId } from './types';

type InstallProgressEvent = {
  phase?: string;
  current?: number;
  total?: number;
  file?: string;
  error?: string;
  done?: boolean;
};

const INSTALL_ETA_PHASES = new Set([
  'libraries',
  'assets',
  'loader_libraries',
  'loader_processors',
  'processors',
]);

export function handleInstallClick(): void {
  const inst = selectedInstance.value;
  if (!inst) return;

  const version = selectedVersion.value;
  const target = version?.needs_install || version?.id || inst.version_id;
  const loader = version?.loader_component_id && version?.loader_build_id
    ? {
        componentId: version.loader_component_id as LoaderComponentId,
        buildId: version.loader_build_id,
        minecraftVersion: version.inherits_from || '',
        loaderVersion: inferLoaderVersionFromBuildId(version.loader_build_id),
        versionId: target,
      }
    : parseLoaderFromId(target, version?.inherits_from || '');
  if (loader) {
    installLoaderVersion({
      component_id: loader.componentId,
      component_name: '',
      build_id: loader.buildId,
      minecraft_version: loader.minecraftVersion,
      loader_version: loader.loaderVersion,
      version_id: loader.versionId,
      stable: false,
      prerelease: false,
      recommended: false,
      latest: false,
      strategy: '',
      artifact_kind: '',
      installability: '',
    });
    return;
  }

  installVersion(target);
}

export function installVersion(target: string): void {
  if (!target) return;
  const active = installState.value;
  if (active.status === 'active' && active.versionId === target) return;
  enqueueInstall({ versionId: target });
  if (installState.value.status === 'idle') processNextInstall();
}

function parseLoaderFromId(
  id: string,
  baseVersion: string,
): { componentId: LoaderComponentId; buildId: string; minecraftVersion: string; loaderVersion: string; versionId: string } | null {
  const lo = id.toLowerCase();
  const inferredBase = baseVersion || inferMinecraftVersionFromCompositeId(id);

  if (lo.startsWith('fabric-loader-')) {
    const suffix = inferredBase ? `-${inferredBase}` : '';
    const rest = id.slice('fabric-loader-'.length);
    const loaderVersion = suffix && rest.endsWith(suffix) ? rest.slice(0, -suffix.length) : rest;
    if (loaderVersion && inferredBase) {
      return {
        componentId: 'net.fabricmc.fabric-loader',
        buildId: `fabric:${inferredBase}:${loaderVersion}`,
        minecraftVersion: inferredBase,
        loaderVersion,
        versionId: id,
      };
    }
  }

  if (lo.startsWith('quilt-loader-')) {
    const suffix = inferredBase ? `-${inferredBase}` : '';
    const rest = id.slice('quilt-loader-'.length);
    const loaderVersion = suffix && rest.endsWith(suffix) ? rest.slice(0, -suffix.length) : rest;
    if (loaderVersion && inferredBase) {
      return {
        componentId: 'org.quiltmc.quilt-loader',
        buildId: `quilt:${inferredBase}:${loaderVersion}`,
        minecraftVersion: inferredBase,
        loaderVersion,
        versionId: id,
      };
    }
  }

  const forgeIndex = lo.lastIndexOf('-forge-');
  if (forgeIndex > 0) {
    const minecraftVersion = id.slice(0, forgeIndex);
    const loaderVersion = id.slice(forgeIndex + '-forge-'.length);
    if (minecraftVersion && loaderVersion) {
      return {
        componentId: 'net.minecraftforge',
        buildId: `forge:${minecraftVersion}:${loaderVersion}`,
        minecraftVersion,
        loaderVersion,
        versionId: id,
      };
    }
  }

  if (lo.startsWith('neoforge-')) {
    const loaderVersion = id.slice('neoforge-'.length);
    const minecraftVersion = inferNeoForgeGameVersion(loaderVersion);
    if (loaderVersion && minecraftVersion) {
      return {
        componentId: 'net.neoforged',
        buildId: `neoforge:${minecraftVersion}:${loaderVersion}`,
        minecraftVersion,
        loaderVersion,
        versionId: id,
      };
    }
  }

  return null;
}

function inferNeoForgeGameVersion(loaderVersion: string): string {
  const parts = loaderVersion.split('.', 3);
  if (parts.length < 2) return '';
  if (parts[1] === '0') return `1.${parts[0]}`;
  return `1.${parts[0]}.${parts[1]}`;
}

function inferMinecraftVersionFromCompositeId(id: string): string {
  const snapshot = id.match(/(\d{2}w\d{2}[a-z])$/i);
  if (snapshot) return snapshot[1];

  const prerelease = id.match(/(\d+\.\d+(?:\.\d+)?-(?:pre|rc)\d+)$/i);
  if (prerelease) return prerelease[1];

  const release = id.match(/(\d+\.\d+(?:\.\d+)?)$/);
  if (release) return release[1];

  return '';
}

export function installLoaderVersion(build: LoaderBuildRecord): void {
  if (!build.component_id || !build.build_id || !build.version_id) return;
  const active = installState.value;
  if (active.status === 'active' && active.versionId === build.version_id) return;
  enqueueInstall({
    versionId: build.version_id,
    loader: {
      componentId: build.component_id,
      buildId: build.build_id,
      minecraftVersion: build.minecraft_version,
      loaderVersion: build.loader_version,
    },
  });
  if (installState.value.status === 'idle') processNextInstall();
}

function processNextInstall(): void {
  if (installState.value.status !== 'idle') return;
  const next = dequeueNextInstall();
  if (!next) return;
  if (next.loader) processLoaderInstall(next);
  else processVanillaInstall(next);
}

async function processVanillaInstall(next: InstallItem): Promise<void> {
  startInstall(next.versionId, 'Starting download...');

  try {
    const res = await api('POST', '/install', { version_id: next.versionId });
    if (res.error) {
      showError(res.error);
      await onInstallDone();
      return;
    }
    await connectVanillaEvents(res.install_id, next.versionId);
  } catch (err: unknown) {
    showError(`Install failed: ${errMessage(err)}`);
    await onInstallDone();
  }
}

async function processLoaderInstall(next: InstallItem): Promise<void> {
  if (!next.loader) return;

  startInstall(next.versionId, 'Starting loader install...');

  try {
    const installId = await startLoaderInstall(
      next.loader.componentId,
      next.loader.buildId,
    );
    await connectLoaderEvents(installId, next.versionId);
  } catch (err: unknown) {
    showError(`Loader install failed: ${errMessage(err)}`);
    await onInstallDone();
  }
}

function inferLoaderVersionFromBuildId(buildId: string): string {
  const parts = buildId.split(':');
  return parts[2] || '';
}

function formatCountLabel(base: string, data: InstallProgressEvent): string {
  if (typeof data.current === 'number' && typeof data.total === 'number' && data.total > 0) {
    return `${base} (${data.current}/${data.total})`;
  }
  return base;
}

function progressFraction(data: InstallProgressEvent): number {
  if (typeof data.current !== 'number' || typeof data.total !== 'number' || data.total <= 0) {
    return 0;
  }
  return data.current / data.total;
}

function phaseLabel(data: InstallProgressEvent, loaderInstall: boolean): string {
  switch (data.phase) {
    case 'loader_meta':
      return 'Fetching loader info...';
    case 'loader_json':
      return 'Preparing loader...';
    case 'profile':
      return data.file || 'Preparing loader profile...';
    case 'artifacts':
      return data.file || 'Downloading loader artifacts...';
    case 'loader_libraries':
      return formatCountLabel('Loader libraries', data);
    case 'loader_processors':
    case 'processors':
      return data.file || formatCountLabel('Running processors', data);
    case 'version_json':
      return 'Fetching version info...';
    case 'client_jar':
      return 'Downloading game JAR...';
    case 'libraries':
      return formatCountLabel('Libraries', data);
    case 'asset_index':
      return 'Downloading asset index...';
    case 'assets':
      return formatCountLabel('Assets', data);
    case 'log_config':
      return 'Downloading log config...';
    case 'java_runtime':
      return data.file || 'Preparing Java runtime...';
    case 'done':
      return 'Complete!';
    case 'error':
      return data.error || 'Install failed.';
    default:
      if (typeof data.file === 'string' && data.file.trim()) {
        return data.file;
      }
      return loaderInstall ? `Working on ${data.phase || 'loader install'}...` : `Working on ${data.phase || 'install'}...`;
  }
}

async function connectVanillaEvents(installId: string, versionId: string): Promise<void> {
  const startedAt = Date.now();
  const estimator = createProgressEstimator({ etaPhases: INSTALL_ETA_PHASES });

  const onProgress = async (data: InstallProgressEvent): Promise<void> => {
    let pct = 0;
    let label = phaseLabel(data, false);

    if (data.phase === 'version_json') {
      pct = 2;
    } else if (data.phase === 'client_jar') {
      pct = 7;
    } else if (data.phase === 'libraries') {
      const libraryPct = progressFraction(data);
      pct = 7 + Math.round(libraryPct * 13);
    } else if (data.phase === 'asset_index') {
      pct = 21;
    } else if (data.phase === 'assets') {
      const assetPct = progressFraction(data);
      pct = 21 + Math.round(assetPct * 72);
    } else if (data.phase === 'log_config') {
      pct = 94;
    } else if (data.phase === 'java_runtime') {
      pct = 95;
    } else if (data.phase === 'done') {
      pct = 100;
    } else if (data.phase === 'error') {
      showError(data.error || 'Install failed.');
      await onInstallDone();
      return;
    } else {
      pct = installState.value.status === 'active' ? installState.value.pct : 0;
    }

    updateInstallProgress(pct, estimator.formatLabel(label, data, pct, startedAt));
    if (data.done) await onInstallDone();
  };

  if (hasNativeDesktopRuntime()) {
    const subscription = await onNativeEvent(nativeInstallEventName(installId), (data) => {
      void onProgress(data);
    });
    if (!subscription) throw new Error('native install stream unavailable');
    setInstallEventSource(subscription);
    try {
      await startNativeInstallEvents(installId);
    } catch (err: unknown) {
      subscription.close();
      setInstallEventSource(null);
      throw err;
    }
    return;
  }

  const es = new EventSource(`${API}/install/${installId}/events`);
  setInstallEventSource(es);

  es.addEventListener('progress', (e: MessageEvent) => {
    void onProgress(JSON.parse(e.data));
  });

  es.onerror = () => {
    if (es.readyState !== EventSource.CLOSED) return;
    void (async () => {
      const active = installState.value;
      if (active.status === 'active' && active.versionId === versionId) {
        showError('Install event stream closed unexpectedly');
        await onInstallDone();
      }
    })();
  };
}

async function connectLoaderEvents(installId: string, versionId: string): Promise<void> {
  const startedAt = Date.now();
  const estimator = createProgressEstimator({ etaPhases: INSTALL_ETA_PHASES });
  const onProgress = (data: InstallProgressEvent): void => {
    let pct = 0;
    let label = phaseLabel(data, true);

    if (data.phase === 'loader_meta') {
      pct = 1;
    } else if (data.phase === 'loader_json') {
      pct = 3;
    } else if (data.phase === 'profile') {
      pct = 3;
    } else if (data.phase === 'artifacts') {
      pct = 6;
    } else if (data.phase === 'loader_libraries') {
      const loaderPct = progressFraction(data);
      pct = 3 + Math.round(loaderPct * 7);
    } else if (data.phase === 'loader_processors' || data.phase === 'processors') {
      const processorPct = progressFraction(data);
      pct = 10 + Math.round(processorPct * 10);
    } else if (data.phase === 'version_json') {
      pct = 21;
    } else if (data.phase === 'client_jar') {
      pct = 24;
    } else if (data.phase === 'libraries') {
      const libraryPct = progressFraction(data);
      pct = 24 + Math.round(libraryPct * 10);
    } else if (data.phase === 'asset_index') {
      pct = 35;
    } else if (data.phase === 'assets') {
      const assetPct = progressFraction(data);
      pct = 35 + Math.round(assetPct * 58);
    } else if (data.phase === 'log_config') {
      pct = 94;
    } else if (data.phase === 'java_runtime') {
      pct = 95;
    } else if (data.phase === 'done') {
      pct = 100;
    } else if (data.phase === 'error') {
      onError(data.error || 'Unknown error');
      return;
    } else {
      pct = installState.value.status === 'active' ? installState.value.pct : 0;
    }

    updateInstallProgress(pct, estimator.formatLabel(label, data, pct, startedAt));
    if (data.done) void onInstallDone();
  };

  const onError = (message: string): void => {
    const active = installState.value;
    if (active.status === 'active' && active.versionId === versionId) {
      showError(message);
      void onInstallDone();
    }
  };

  if (hasNativeDesktopRuntime()) {
    const subscription = await onNativeEvent(nativeLoaderInstallEventName(installId), (data) => {
      if (data.phase === 'error' || data.error) {
        onError(data.error || 'Unknown error');
        return;
      }
      onProgress(data);
    });
    if (!subscription) throw new Error('native loader install stream unavailable');
    setInstallEventSource(subscription);
    try {
      await startNativeLoaderInstallEvents(installId);
    } catch (err: unknown) {
      subscription.close();
      setInstallEventSource(null);
      throw err;
    }
    return;
  }

  const es = connectLoaderInstallSSE(
    installId,
    onProgress,
    () => { void onInstallDone(); },
    onError,
  );

  setInstallEventSource(es);
}

async function onInstallDone(): Promise<void> {
  completeInstall();

  try {
    const res = await api('GET', '/versions');
    if (res.error) throw new Error(res.error);
    const nextVersions = res.versions || [];
    versions.value = nextVersions;

    if (catalog.value) {
      const installed = new Set<string>(
        nextVersions.filter((version: { launchable: boolean }) => version.launchable).map((version: { id: string }) => version.id),
      );
      catalog.value = {
        ...catalog.value,
        versions: catalog.value.versions.map((version) => ({
          ...version,
          installed: installed.has(version.id),
        })),
      };
    }
  } catch (err: unknown) {
    showError(`Install completed, but failed to refresh versions: ${errMessage(err)}`);
  }

  processNextInstall();
}
