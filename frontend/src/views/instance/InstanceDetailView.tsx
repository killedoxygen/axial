import type { JSX } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { Icon } from '../../ui/Icons';
import { Button, IconButton, Pill } from '../../ui/Atoms';
import { InstanceTile, artSeedFor } from '../../ui/InstanceVisual';
import { openContextMenu } from '../../ui/ContextMenu';
import { instances, launchNotices, launchState, runningSessions, versionById } from '../../store';
import { navigate } from '../../ui-state';
import { selectInstance } from '../../actions';
import { launchGame, killGame } from '../../launch';
import { handleInstallClick, retryFailedInstall } from '../../install';
import { errMessage } from '../../utils';
import { instanceInstallStatus } from '../../instance-install-status';
import type { EnrichedInstance } from '../../types-instance';
import { fmtJoined, fmtRelative } from './format';
import { fetchInstanceResources, type ResourceLoadState } from './resources';
import { LOG_RESOURCE_POLL_MS } from './logs';
import { deleteInstanceFlow, duplicateInstance, openInstanceFolder, renameInstance } from './instance-actions';
import { ModsPane } from './tabs/ModsPane';
import { WorldsPane } from './tabs/WorldsPane';
import { ScreenshotsPane } from './tabs/ScreenshotsPane';
import { LogsPane } from './tabs/LogsPane';
import { SettingsPane } from './tabs/SettingsPane';
import { InstallBarrierPane, LaunchOutcomeNotice, LaunchSplitButton } from './components/launch';

export { deleteInstanceFlow, duplicateInstance, openInstanceFolder, renameInstance } from './instance-actions';

type Tab = 'mods' | 'worlds' | 'screenshots' | 'logs' | 'settings';

const TABS: Array<{ id: Tab; icon: string; label: string }> = [
  { id: 'mods', icon: 'puzzle', label: 'Mods' },
  { id: 'worlds', icon: 'globe', label: 'Worlds' },
  { id: 'screenshots', icon: 'image', label: 'Screenshots' },
  { id: 'logs', icon: 'terminal', label: 'Logs' },
  { id: 'settings', icon: 'settings', label: 'Settings' },
];

function fmtElapsed(startedAt: string | undefined, now: number): string {
  const start = startedAt ? Date.parse(startedAt) : NaN;
  if (!Number.isFinite(start)) return '0:00';
  const secs = Math.max(0, Math.floor((now - start) / 1000));
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const r = String(secs % 60).padStart(2, '0');
  return h > 0 ? `${h}:${String(m).padStart(2, '0')}:${r}` : `${m}:${r}`;
}

function defaultTabFor(inst: EnrichedInstance | undefined): Tab {
  return inst?.version_display.supports_mods ? 'mods' : 'worlds';
}

export function InstanceDetailView({ id }: { id: string }): JSX.Element {
  const inst = instances.value.find((i) => i.id === id) as EnrichedInstance | undefined;
  const userSelectedTab = useRef(false);
  const [tab, setTab] = useState<Tab>(() => defaultTabFor(inst));
  const [resources, setResources] = useState<ResourceLoadState>({ status: 'loading', data: null });
  const [now, setNow] = useState(() => Date.now());
  const running = inst ? !!runningSessions.value[inst.id] : false;
  const session = inst ? runningSessions.value[inst.id] : undefined;
  const launch = launchState.value;
  const preparing = inst && launch.status === 'preparing' && launch.instanceId === inst.id ? launch : null;
  const selectTab = (next: Tab): void => {
    userSelectedTab.current = true;
    setTab(next);
  };

  const reloadResources = (): void => {
    if (!inst) return;
    setResources((current) => ({ status: 'loading', data: current.data ?? null }));
    void fetchInstanceResources(inst.id)
      .then((data) => setResources({ status: 'ready', data }))
      .catch((err) =>
        setResources((current) => ({
          status: 'error',
          data: current.data ?? null,
          error: errMessage(err),
        })),
      );
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
    return () => {
      alive = false;
    };
  }, [inst?.id]);

  useEffect(() => {
    userSelectedTab.current = false;
  }, [id]);

  useEffect(() => {
    if (!inst || userSelectedTab.current) return;
    setTab(defaultTabFor(inst));
  }, [inst?.id, inst?.version_id]);

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

  useEffect(() => {
    if (!running) return;
    setNow(Date.now());
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [running]);

  if (!inst) {
    return (
      <div class="cp-view-page">
        <div class="cp-empty">
          <Icon name="stack" size={36} color="var(--text-mute)" />
          <h2>Instance not found</h2>
          <p>That instance might have been deleted.</p>
          <Button icon="chevron-left" onClick={() => navigate({ name: 'instances' })}>
            Back to instances
          </Button>
        </div>
      </div>
    );
  }

  const v = versionById(inst.version_id);
  const showModsTab = inst.version_display.supports_mods;
  const activeTab: Tab = !showModsTab && tab === 'mods' ? 'worlds' : tab;
  const visibleTabs = showModsTab ? TABS : TABS.filter((t) => t.id !== 'mods');
  const auroraHue = artSeedFor(inst) % 360;
  const launchAction = inst.launch_action;
  const installStatus = instanceInstallStatus(inst, v);
  const installTarget = installStatus.target;
  const installProgress = installStatus.progress;
  const installQueued = installStatus.state === 'queued';
  const installQueuedView = installStatus.queuedItem;
  const matchingInstallFailure = installStatus.failure;
  const installLabel = installStatus.label;
  const installLocked =
    launchAction.primary_action === 'install' && (installStatus.installing || Boolean(matchingInstallFailure));

  const onPlay = (): void => {
    selectInstance(inst.id);
    void launchGame();
  };
  const onInstall = (): void => {
    selectInstance(inst.id);
    handleInstallClick(installStatus.item);
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

  const launchNotice = launchNotices.value[inst.id];

  return (
    <div class="cp-instance-view" data-running={running} style={{ ['--cp-aurora-h' as any]: auroraHue }}>
      <div class="cp-instance-stage" aria-hidden="true">
        <div class="cp-instance-aurora-sheet">
          <div class="cp-instance-aurora cp-instance-aurora--b1" />
          <div class="cp-instance-aurora cp-instance-aurora--b2" />
          <div class="cp-instance-aurora cp-instance-aurora--b3" />
          <div class="cp-instance-aurora cp-instance-aurora--b4" />
        </div>
      </div>

      <div class="cp-view-page cp-instance-page">
        <header class="cp-instance-hero">
          <div class="cp-instance-hero-tile">
            <InstanceTile inst={inst} radius={18} />
          </div>
          <div class="cp-instance-hero-id">
            <div class="cp-instance-hero-kicker">
              <Pill>
                {inst.version_display.loader_label}
                {inst.version_display.loader_version_label ? ` ${inst.version_display.loader_version_label}` : ''}
              </Pill>
              <span class="cp-instance-hero-mc">Minecraft {inst.version_display.minecraft_label}</span>
            </div>
            <h1 class="cp-instance-hero-title">{inst.name}</h1>
            <div class="cp-instance-hero-meta">
              <span class="cp-instance-status" data-running={running}>
                {running ? 'Playing now' : 'Ready'}
              </span>
              <span class="cp-instance-hero-meta-sep" aria-hidden="true">
                ·
              </span>
              <span>
                Last played <b>{fmtRelative(inst.last_played_at)}</b>
              </span>
              <span class="cp-instance-hero-meta-sep" aria-hidden="true">
                ·
              </span>
              <span>
                Created <b>{fmtJoined(inst.created_at)}</b>
              </span>
            </div>
          </div>
          <div class="cp-instance-hero-actions">
            <div class="cp-instance-launch">
              {running ? (
                <div class="cp-session">
                  <span class="cp-session-time">
                    <span class="cp-session-dot" aria-hidden="true" />
                    <span>{fmtElapsed(session?.launchedAt, now)}</span>
                  </span>
                  <button class="cp-session-stop" type="button" onClick={onStop}>
                    <Icon name="stop" size={13} />
                    Stop
                  </button>
                </div>
              ) : (
                <LaunchSplitButton
                  inst={inst}
                  launchAction={launchAction}
                  installQueued={installQueued}
                  installQueuedView={installQueuedView}
                  installProgress={installProgress}
                  onLaunch={onPlay}
                  onInstall={onInstall}
                  onOpenLogs={() => selectTab('logs')}
                  onOpenSettings={() => selectTab('settings')}
                  preparing={preparing}
                />
              )}
            </div>
            <IconButton
              icon="dots"
              tooltip="More"
              onClick={(e) =>
                openContextMenu(e, [
                  { icon: 'folder', label: 'Open folder', onSelect: () => void openInstanceFolder(inst.id) },
                  {
                    icon: 'folder',
                    label: 'Open resource packs folder',
                    onSelect: () => void openInstanceFolder(inst.id, 'resourcepacks'),
                  },
                  {
                    icon: 'folder',
                    label: 'Open shader packs folder',
                    onSelect: () => void openInstanceFolder(inst.id, 'shaderpacks'),
                  },
                  { icon: 'copy', label: 'Duplicate', onSelect: () => void duplicateInstance(inst) },
                  { icon: 'edit', label: 'Rename', onSelect: () => void renameInstance(inst) },
                  { label: '', onSelect: () => {}, divider: true },
                  {
                    icon: 'trash',
                    label: 'Delete',
                    onSelect: () => void deleteInstanceFlow(inst, () => navigate({ name: 'instances' })),
                    danger: true,
                  },
                ])
              }
            />
          </div>
        </header>

        {!installLocked && (
          <div class="cp-instance-tabs" role="tablist">
            {visibleTabs.map((t) => {
              const count = tabCount(t.id);
              return (
                <button
                  key={t.id}
                  role="tab"
                  aria-selected={activeTab === t.id}
                  data-active={activeTab === t.id}
                  aria-label={t.label}
                  title={t.label}
                  onClick={() => selectTab(t.id)}
                >
                  <Icon name={t.icon} size={15} />
                  <span class="cp-tab-label">{t.label}</span>
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
            installQueuedView={installQueuedView}
            installProgress={installProgress}
            installFailure={matchingInstallFailure}
            onRetryInstall={retryFailedInstall}
          />
        )}
        {!installLocked && activeTab === 'mods' && (
          <ModsPane inst={inst} resources={resources} onRefresh={reloadResources} />
        )}
        {!installLocked && activeTab === 'worlds' && (
          <WorldsPane inst={inst} resources={resources} onRefresh={reloadResources} />
        )}
        {!installLocked && activeTab === 'screenshots' && (
          <ScreenshotsPane inst={inst} resources={resources} onRefresh={reloadResources} />
        )}
        {!installLocked && activeTab === 'logs' && (
          <LogsPane inst={inst} resources={resources} running={running} onRefresh={reloadResources} />
        )}
        {!installLocked && activeTab === 'settings' && <SettingsPane inst={inst} />}
      </div>
    </div>
  );
}
