import { api, API } from './api';
import { catalog, versions } from './store';
import { isWailsRuntime } from './native';
import type { Version } from './types';

let versionWatcher: EventSource | null = null;
let versionPollTimer: ReturnType<typeof setInterval> | null = null;

function applyVersions(nextVersions: Version[]): void {
  versions.value = nextVersions;

  if (catalog.value) {
    const installed = new Set<string>(
      nextVersions.filter((version) => version.launchable).map((version) => version.id),
    );
    catalog.value = {
      ...catalog.value,
      versions: catalog.value.versions.map((version) => ({
        ...version,
        installed: installed.has(version.id),
      })),
    };
  }
}

async function pollVersions(): Promise<void> {
  try {
    const res = await api('GET', '/versions');
    applyVersions(res.versions || []);
  } catch {}
}

export function watchVersions(): void {
  if (versionWatcher) versionWatcher.close();
  if (versionPollTimer) {
    clearInterval(versionPollTimer);
    versionPollTimer = null;
  }

  if (isWailsRuntime()) {
    void pollVersions();
    versionPollTimer = setInterval(() => { void pollVersions(); }, 5000);
    return;
  }

  const es = new EventSource(`${API}/versions/watch`);
  versionWatcher = es;

  es.addEventListener('versions_changed', (e: MessageEvent) => {
    try {
      const data: { versions?: Version[] } = JSON.parse(e.data);
      applyVersions(data.versions || []);
    } catch {}
  });

  es.onerror = (): void => {
    es.close();
    versionWatcher = null;
    setTimeout(watchVersions, 5000);
  };
}
