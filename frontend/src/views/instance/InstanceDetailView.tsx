import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Icon } from '../../ui/Icons';
import { Button, IconButton, Pill } from '../../ui/Atoms';
import { useTheme } from '../../hooks/use-theme';
import { InstanceArt } from '../../art/InstanceArt';
import { openContextMenu } from '../../ui/ContextMenu';
import { installFailure, installQueue, installState, instances, launchNotices, launchState, runningSessions, versions } from '../../store';
import { navigate } from '../../ui-state';
import { isActiveInstallItem, isSameInstallItem, selectInstance } from '../../actions';
import { launchGame, killGame } from '../../launch';
import { handleInstallClick, retryFailedInstall } from '../../install';
import { formatInstallItemLabel } from '../../install-labels';
import { errMessage, supportsMods } from '../../utils';
import { loaderKeyFromVersion, LOADER_LABELS } from '../create/defaults';
import type { EnrichedInstance, InstallItem, Version } from '../../types';
import { fmtJoined, fmtRelative } from './format';
import { fetchInstanceResources, type ResourceLoadState } from './resources';
import { LOG_RESOURCE_POLL_MS } from './logs';
import { deleteInstanceFlow, duplicateInstance, openInstanceFolder, renameInstance } from './instance-actions';
import { OverviewPane } from './overview/OverviewPane';
import { LogsCard } from './overview/LogsCard';
import { ModsPane } from './tabs/ModsPane';
import { WorldsPane } from './tabs/WorldsPane';
import { ScreenshotsPane } from './tabs/ScreenshotsPane';
import { LogsPane } from './tabs/LogsPane';
import { SettingsPane } from './tabs/SettingsPane';
import { InstallBarrierPane, LaunchOutcomeNotice, LaunchSplitButton } from './components/launch';

export { deleteInstanceFlow, duplicateInstance, openInstanceFolder, renameInstance } from './instance-actions';

type Tab = 'overview' | 'mods' | 'worlds' | 'screenshots' | 'logs' | 'settings';

const TABS: Array<{ id: Tab; icon: string; label: string }> = [
  { id: 'overview', icon: 'info', label: 'Overview' },
  { id: 'mods', icon: 'puzzle', label: 'Mods' },
  { id: 'worlds', icon: 'globe', label: 'Worlds' },
  { id: 'screenshots', icon: 'image', label: 'Screenshots' },
  { id: 'logs', icon: 'terminal', label: 'Logs' },
  { id: 'settings', icon: 'settings', label: 'Settings' },
];

function loaderLabel(v: Version | undefined): string {
  return LOADER_LABELS[loaderKeyFromVersion(v)];
}

function installTargetFor(inst: EnrichedInstance, version: Version | undefined): string {
  return version?.needs_install || version?.id || inst.version_id;
}

function installItemFor(inst: EnrichedInstance, version: Version | undefined): InstallItem {
  const versionId = installTargetFor(inst, version);
  if (!version?.loader) return { versionId };
  return {
    versionId,
    loader: {
      componentId: version.loader.component_id,
      buildId: version.loader.build_id,
      minecraftVersion: version.inherits_from || '',
      loaderVersion: version.loader.loader_version,
    },
  };
}

export function InstanceDetailView({ id }: { id: string }): JSX.Element {
  const theme = useTheme();
  const inst = instances.value.find(i => i.id === id) as EnrichedInstance | undefined;
  const [tab, setTab] = useState<Tab>('overview');
  const [resources, setResources] = useState<ResourceLoadState>({ status: 'loading', data: null });
  const running = inst ? !!runningSessions.value[inst.id] : false;
  const launch = launchState.value;
  const preparing = inst && launch.status === 'preparing' && launch.instanceId === inst.id ? launch : null;

  const reloadResources = (): void => {
    if (!inst) return;
    setResources((current) => ({ status: 'loading', data: current.data ?? null }));
    void fetchInstanceResources(inst.id)
      .then((data) => setResources({ status: 'ready', data }))
      .catch((err) => setResources((current) => ({
        status: 'error',
        data: current.data ?? null,
        error: errMessage(err),
      })));
  };

  useEffect(() => {
    if (!inst) return;
    let alive = true;
    setResources({ status: 'loading', data: null });
    void fetchInstanceResources(inst.id)
      .then((data) => {
        if (alive) setResources({ status: 'ready', data });
      })
      .catch((err) => {
        if (alive) setResources({ status: 'error', data: null, error: errMessage(err) });
      });
    return () => { alive = false; };
  }, [inst?.id]);

  useEffect(() => {
    if (!inst || !running) return;
    let alive = true;
    const refreshQuietly = (): void => {
      void fetchInstanceResources(inst.id)
        .then((data) => {
          if (alive) setResources({ status: 'ready', data });
        })
        .catch((err) => {
          if (alive) {
            setResources((current) => ({
              status: 'error',
              data: current.data ?? null,
              error: errMessage(err),
            }));
          }
        });
    };
    refreshQuietly();
    const timer = window.setInterval(refreshQuietly, LOG_RESOURCE_POLL_MS);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [inst?.id, running]);

  if (!inst) {
    return (
      <div class="cp-view-page">
        <div class="cp-empty">
          <Icon name="cube" size={36} color="var(--text-mute)" />
          <h2>Instance not found</h2>
          <p>That instance might have been deleted.</p>
          <Button icon="chevron-left" onClick={() => navigate({ name: 'instances' })}>Back to instances</Button>
        </div>
      </div>
    );
  }

  const v = versions.value.find(x => x.id === inst.version_id);
  const showModsTab = supportsMods(v);
  const activeTab: Tab = !showModsTab && tab === 'mods' ? 'overview' : tab;
  const visibleTabs = showModsTab ? TABS : TABS.filter((t) => t.id !== 'mods');
  const mcVer = v?.minecraft_meta.display_hint || v?.minecraft_meta.display_name || 'unknown';
  const canLaunch = Boolean(v?.launchable);
  const installTarget = installTargetFor(inst, v);
  const installItem = installItemFor(inst, v);
  const install = installState.value;
  const installProgress = install.status === 'active' && isActiveInstallItem(installItem)
    ? {
        pct: install.pct,
        label: install.label,
        displayName: install.displayName,
        remainingSeconds: install.remainingSeconds,
        remainingSecondsUpdatedAt: install.remainingSecondsUpdatedAt,
      }
    : null;
  const queuedInstallIndex = installQueue.value.findIndex(item => isSameInstallItem(item, installItem));
  const queuedInstall = queuedInstallIndex >= 0 ? installQueue.value[queuedInstallIndex] : undefined;
  const installQueued = !installProgress && Boolean(queuedInstall);
  const installQueuePosition = installQueued ? queuedInstallIndex + 1 : undefined;
  const installQueueCount = installQueued ? installQueue.value.length : undefined;
  const failedInstall = installFailure.value;
  const matchingInstallFailure = failedInstall && isSameInstallItem(failedInstall.item, installItem)
    ? failedInstall
    : null;
  const installLabel = installProgress?.displayName
    || (queuedInstall ? formatInstallItemLabel(queuedInstall) : matchingInstallFailure?.displayName || installTarget);
  const installLocked = !canLaunch && (Boolean(installProgress) || installQueued || Boolean(matchingInstallFailure));

  const onPlay = (): void => {
    selectInstance(inst.id);
    void launchGame();
  };
  const onInstall = (): void => {
    selectInstance(inst.id);
    handleInstallClick();
  };
  const onStop = (): void => {
    selectInstance(inst.id);
    void killGame();
  };

  const tabCount = (t: Tab): number | undefined => {
    if (t === 'mods') {
      if (!showModsTab) return undefined;
      const n = resources.data?.mods_count ?? inst.mods_count ?? 0;
      return n > 0 ? n : undefined;
    }
    if (t === 'worlds') {
      const n = resources.data?.worlds_count ?? inst.saves_count ?? 0;
      return n > 0 ? n : undefined;
    }
    if (t === 'screenshots') {
      const n = resources.data?.screenshots_count ?? 0;
      return n > 0 ? n : undefined;
    }
    if (t === 'logs') {
      const n = resources.data?.logs_count ?? 0;
      return n > 0 ? n : undefined;
    }
    return undefined;
  };

  const loaderVer = v?.loader?.loader_version ?? '';
  const launchNotice = launchNotices.value[inst.id];

  return (
    <div class={`cp-instance-page${activeTab === 'overview' ? ' cp-instance-page--overview' : ''}`}>
      <div class="cp-instance-cover">
        <InstanceArt instance={inst} aspect="banner" className="cp-instance-cover-art" />
        <div class="cp-instance-cover-vignette" aria-hidden="true" />
        <div class="cp-instance-cover-glow" aria-hidden="true" />
      </div>

      <div class="cp-instance-titlebar">
        <div class="cp-instance-titlebar-row">
          <div class="cp-instance-titlebar-left">
            <div class="cp-instance-avatar">
              <InstanceArt instance={inst} aspect="square" radius={theme.r.lg} />
            </div>
            <div class="cp-instance-titlebar-text">
              <div class="cp-instance-pills-row">
                <Pill>{loaderLabel(v)}{loaderVer ? ` ${loaderVer}` : ''}</Pill>
                <span class="cp-instance-mc-version">Minecraft {mcVer}</span>
              </div>
              <h1 class="cp-instance-title">{inst.name}</h1>
              <div class="cp-instance-subtitle">
                <span>Last played <b>{fmtRelative(inst.last_played_at)}</b></span>
                <span class="cp-instance-subtitle-sep" aria-hidden="true">·</span>
                <span>Created <b>{fmtJoined(inst.created_at)}</b></span>
              </div>
            </div>
          </div>
          <div class="cp-instance-actions">
            <div class="cp-instance-launch">
              {running ? (
                <Button variant="secondary" size="lg" icon="stop" onClick={onStop}>Stop</Button>
              ) : (
                <LaunchSplitButton
                  inst={inst}
                  canLaunch={canLaunch}
                  installQueued={installQueued}
                  installProgress={installProgress}
                  onLaunch={onPlay}
                  onInstall={onInstall}
                  onOpenLogs={() => setTab('logs')}
                  onOpenSettings={() => setTab('settings')}
                  preparing={preparing}
                />
              )}
            </div>
            <IconButton icon="dots" tooltip="More"
              onClick={(e) => openContextMenu(e, [
                { icon: 'folder', label: 'Open folder', onSelect: () => void openInstanceFolder(inst.id) },
                { icon: 'folder', label: 'Open resource packs folder', onSelect: () => void openInstanceFolder(inst.id, 'resourcepacks') },
                { icon: 'folder', label: 'Open shader packs folder', onSelect: () => void openInstanceFolder(inst.id, 'shaderpacks') },
                { icon: 'copy', label: 'Duplicate', onSelect: () => void duplicateInstance(inst) },
                { icon: 'edit', label: 'Rename', onSelect: () => void renameInstance(inst) },
                { label: '', onSelect: () => {}, divider: true },
                { icon: 'trash', label: 'Delete', onSelect: () => void deleteInstanceFlow(inst, () => navigate({ name: 'instances' })), danger: true },
              ])} />
          </div>
        </div>
      </div>

      {!installLocked && (
        <div class="cp-instance-tabs" role="tablist">
          {visibleTabs.map(t => {
            const count = tabCount(t.id);
            return (
              <button
                key={t.id}
                role="tab"
                aria-selected={activeTab === t.id}
                data-active={activeTab === t.id}
                onClick={() => setTab(t.id)}
              >
                <Icon name={t.icon} size={15} />
                {t.label}
                {count != null && <span class="cp-tab-count">{count}</span>}
              </button>
            );
          })}
        </div>
      )}

      {launchNotice && <LaunchOutcomeNotice inst={inst} notice={launchNotice} />}

      {installLocked && (
        <InstallBarrierPane
          installTarget={installTarget}
          installLabel={installLabel}
          installQueued={installQueued}
          installProgress={installProgress}
          installFailure={matchingInstallFailure}
          installQueuePosition={installQueuePosition}
          installQueueCount={installQueueCount}
          onRetryInstall={retryFailedInstall}
        />
      )}
      {!installLocked && activeTab === 'overview' && (
        <>
          <OverviewPane
            inst={inst}
            resources={resources.data}
            onRefreshResources={reloadResources}
            running={running}
            onLaunch={onPlay}
            onStop={onStop}
            onOpenWorlds={() => setTab('worlds')}
            onOpenLogs={() => setTab('logs')}
          />
          <div class="cp-instance-bottom">
            <LogsCard instanceId={inst.id} resources={resources.data} running={running} onOpenLogs={() => setTab('logs')} />
          </div>
        </>
      )}
      {!installLocked && activeTab === 'mods' && <ModsPane inst={inst} resources={resources} onRefresh={reloadResources} />}
      {!installLocked && activeTab === 'worlds' && <WorldsPane inst={inst} resources={resources} onRefresh={reloadResources} />}
      {!installLocked && activeTab === 'screenshots' && <ScreenshotsPane inst={inst} resources={resources} onRefresh={reloadResources} />}
      {!installLocked && activeTab === 'logs' && <LogsPane inst={inst} resources={resources} running={running} onRefresh={reloadResources} />}
      {!installLocked && activeTab === 'settings' && <SettingsPane inst={inst} />}
    </div>
  );
}
