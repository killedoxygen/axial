import { api, API } from './api';
import { showError, errMessage } from './utils';
import { startLoaderInstall, connectLoaderInstallSSE } from './loaders/api';
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

async function connectVanillaEvents(installId: string, versionId: string): Promise<void> {
  const startedAt = Date.now();

  const onProgress = async (data: any): Promise<void> => {
    let pct = 0;
    let label = '';

    if (data.phase === 'version_json') {
      pct = 2;
      label = 'Fetching version info...';
    } else if (data.phase === 'client_jar') {
      pct = 7;
      label = 'Downloading game JAR...';
    } else if (data.phase === 'libraries') {
      const libraryPct = data.total > 0 ? data.current / data.total : 0;
      pct = 7 + Math.round(libraryPct * 13);
      label = `Libraries (${data.current}/${data.total})`;
    } else if (data.phase === 'asset_index') {
      pct = 21;
      label = 'Downloading asset index...';
    } else if (data.phase === 'assets') {
      const assetPct = data.total > 0 ? data.current / data.total : 0;
      pct = 21 + Math.round(assetPct * 72);
      label = `Assets (${data.current}/${data.total})`;
    } else if (data.phase === 'log_config') {
      pct = 94;
      label = 'Downloading log config...';
    } else if (data.phase === 'done') {
      pct = 100;
      label = 'Complete!';
    } else if (data.phase === 'error') {
      showError(data.error);
      await onInstallDone();
      return;
    }

    updateInstallProgress(pct, appendETA(label, pct, startedAt));
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
  const onProgress = (data: any): void => {
    let pct = 0;
    let label = '';

    if (data.phase === 'loader_meta') {
      pct = 1;
      label = 'Fetching loader info...';
    } else if (data.phase === 'loader_json') {
      pct = 3;
      label = 'Preparing loader...';
    } else if (data.phase === 'loader_libraries') {
      const loaderPct = data.total > 0 ? data.current / data.total : 0;
      pct = 3 + Math.round(loaderPct * 7);
      label = `Loader libraries (${data.current}/${data.total})`;
    } else if (data.phase === 'loader_processors') {
      const processorPct = data.total > 0 ? data.current / data.total : 0;
      pct = 10 + Math.round(processorPct * 10);
      label = data.file || `Processing (${data.current}/${data.total})`;
    } else if (data.phase === 'version_json') {
      pct = 21;
      label = 'Fetching version info...';
    } else if (data.phase === 'client_jar') {
      pct = 24;
      label = 'Downloading game JAR...';
    } else if (data.phase === 'libraries') {
      const libraryPct = data.total > 0 ? data.current / data.total : 0;
      pct = 24 + Math.round(libraryPct * 10);
      label = `Libraries (${data.current}/${data.total})`;
    } else if (data.phase === 'asset_index') {
      pct = 35;
      label = 'Downloading asset index...';
    } else if (data.phase === 'assets') {
      const assetPct = data.total > 0 ? data.current / data.total : 0;
      pct = 35 + Math.round(assetPct * 58);
      label = `Assets (${data.current}/${data.total})`;
    } else if (data.phase === 'log_config') {
      pct = 94;
      label = 'Downloading log config...';
    } else if (data.phase === 'done') {
      pct = 100;
      label = 'Complete!';
    }

    updateInstallProgress(pct, appendETA(label, pct, startedAt));
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

function appendETA(label: string, pct: number, startedAt: number): string {
  if (pct <= 5 || pct >= 100) return label;
  const elapsed = (Date.now() - startedAt) / 1000;
  const remaining = (elapsed / pct) * (100 - pct);
  if (remaining < 60) return `${label} — ~${Math.ceil(remaining)}s left`;
  return `${label} — ~${Math.ceil(remaining / 60)}m left`;
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
