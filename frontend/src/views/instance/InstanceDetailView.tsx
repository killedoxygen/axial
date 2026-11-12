import type { JSX } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { Button, Chip, Empty, IconButton, Status, Tabs, type TabItem } from '../../ui/Atoms';
import { artSeedFor } from '../../ui/InstanceVisual';
import { Vista } from '../../vista/Vista';
import { openContextMenu } from '../../ui/ContextMenu';
import { instances, launchNotices, launchSessions, launchState, versionById } from '../../store';
import { navigate, resetViewScroll } from '../../ui-state';
import { selectInstance } from '../../actions';
import { launchGame, killGame } from '../../launch';
import { handleInstallClick, retryFailedInstall } from '../../machines/downloads';
import { errMessage } from '../../utils';
import { fmtRelative } from '../../format';
import { instanceInstallStatus } from '../../instance-install-status';
import { refreshInstanceReadiness } from '../../instance-readiness';
import { resumeInstanceSetup } from '../../instance-setup';
import {
  launchActionPresentation,
  launchSessionActivityLabel,
  launchSessionCanStop,
  launchSessionHasLiveProcess,
  launchSessionIsPlaying,
} from '../../launch-presenters';
import type { EnrichedInstance } from '../../types-instance';
import { fetchInstanceResources, type ResourceLoadState } from './resources';
import { LOG_RESOURCE_POLL_MS } from './logs';
import { deleteInstanceFlow, duplicateInstance, openInstanceFolder, renameInstance } from './instance-actions';
import { ModsPane } from './tabs/ModsPane';
import { PacksPane } from './tabs/PacksPane';
import { WorldsPane } from './tabs/WorldsPane';
import { ScreenshotsPane } from './tabs/ScreenshotsPane';
import { LogsPane } from './tabs/LogsPane';
import { SettingsPane } from './tabs/SettingsPane';
import { InstallBarrierPane, LaunchOutcomeNotice, LaunchSplitButton } from './components/launch';
import { PackChips } from './components/pack-origin';
import { cachedModProvenance } from './mod-provenance-cache';

type Tab = 'mods' | 'resourcepacks' | 'shaderpacks' | 'worlds' | 'screenshots' | 'logs' | 'settings';
type TabSelection = { instanceId: string; tab: Tab } | null;

const TABS: TabItem<Tab>[] = [
  { id: 'mods', icon: 'puzzle', label: 'Mods' },
  { id: 'resourcepacks', icon: 'image', label: 'Resource packs' },
  { id: 'shaderpacks', icon: 'palette', label: 'Shaders' },
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
  return <InstanceDetailContent key={id} id={id} />;
}

function InstanceDetailContent({ id }: { id: string }): JSX.Element {
  const inst = instances.value.find((i) => i.id === id);
  const [selectedTab, setSelectedTab] = useState<TabSelection>(null);
  const selectedTabForCurrentInstance = selectedTab?.instanceId === id ? selectedTab.tab : null;
  const [resources, setResources] = useState<ResourceLoadState>({ status: 'loading', data: null });
  const resourceRequest = useRef(0);
  const [now, setNow] = useState(() => Date.now());
  const session = inst ? launchSessions.value[inst.id] : undefined;
  const sessionActive = Boolean(session);
  const playing = launchSessionIsPlaying(session);
  const processLive = launchSessionHasLiveProcess(session);
  const canStop = launchSessionCanStop(session);
  const sessionLabel = launchSessionActivityLabel(session);
  const launch = launchState.value;
  const preparing = inst && launch.status === 'preparing' && launch.instanceId === inst.id ? launch : null;
  const selectTab = (next: Tab): void => {
    setSelectedTab({ instanceId: id, tab: next });
    resetViewScroll();
  };

  const reloadResources = (quiet = false): void => {
    if (!inst) return;
    const request = ++resourceRequest.current;
    if (!quiet) {
      setResources((current) => ({ status: 'loading', data: current.data ?? null }));
      void refreshInstanceReadiness(inst.id, {
        isCurrent: () => request === resourceRequest.current,
        retry: false,
      }).catch(() => {});
    }
    void fetchInstanceResources(inst.id)
      .then((data) => {
        if (request === resourceRequest.current) setResources({ status: 'ready', data });
      })
      .catch((err) => {
        if (request !== resourceRequest.current) return;
        setResources((current) => ({
          status: 'error',
          data: current.data ?? null,
          error: errMessage(err),
        }));
      });
  };

  // Setup and install hold the instance directory, so reread once the launch state settles.
  useEffect(() => {
    if (!inst) return;
    reloadResources();
    const timer = processLive ? window.setInterval(() => reloadResources(true), LOG_RESOURCE_POLL_MS) : 0;
    return () => {
      resourceRequest.current += 1;
      if (timer) window.clearInterval(timer);
    };
  }, [inst?.id, processLive, inst?.launch_action.state_id]);

  useEffect(() => {
    if (!playing) return;
    setNow(Date.now());
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [playing]);

  if (!inst) {
    return (
      <div class="cp-view-page">
        <Empty
          title="Instance not found"
          actions={
            <Button variant="secondary" icon="chevron-left" onClick={() => navigate({ name: 'instances' })}>
              Back to instances
            </Button>
          }
        >
          That instance might have been deleted.
        </Empty>
      </div>
    );
  }

  const v = versionById(inst.version_id);
  const showModsTab = inst.version_display.supports_mods;
  const currentTab = selectedTabForCurrentInstance ?? defaultTabFor(inst);
  // Mods and shader loaders both need a mod loader; resource packs work everywhere.
  const modTabs: Tab[] = ['mods', 'shaderpacks'];
  const activeTab: Tab = !showModsTab && modTabs.includes(currentTab) ? 'worlds' : currentTab;
  const visibleTabs = showModsTab ? TABS : TABS.filter((t) => !modTabs.includes(t.id));
  const launchAction = inst.launch_action;
  const installStatus = instanceInstallStatus(inst, v);
  const installTarget = installStatus.target;
  // Between an install ending and the instance being re-read, its readiness is from before the install: the
  // button keeps working rather than flashing that stale answer.
  const installProgress =
    installStatus.progress ??
    (installStatus.settling && !inst.launch_action.launchable ? { pct: 0, label: 'Finishing up' } : null);
  const installQueued = installStatus.state === 'queued';
  const installQueuedView = installStatus.queuedItem;
  const matchingInstallFailure = installStatus.failure;
  const installLabel = installStatus.label;
  const installLocked =
    launchAction.primary_action === 'install' && (installStatus.installing || Boolean(matchingInstallFailure));
  const launchPresentation = launchActionPresentation({
    launchAction,
    installQueued,
    installQueuedView,
    installProgress,
    preparing,
  });
  // Status only when it says something the Launch button does not: ready, install and progress are the button itself.
  const failureTitle = launchAction.primary_action === 'install' ? matchingInstallFailure?.viewModel.title : undefined;
  const quiet = launchAction.launchable || (launchAction.primary_action === 'install' && !failureTitle);
  const status = session ? (
    playing ? (
      <Status eyes="open" tone="accent">
        {sessionLabel} <span class="cp-instance-clock">{fmtElapsed(session.launchedAt, now)}</span>
      </Status>
    ) : (
      <Status busy>{sessionLabel}</Status>
    )
  ) : quiet || launchPresentation.progress || installQueued ? null : failureTitle ? (
    <Status eyes="x" tone="err">
      {failureTitle}
    </Status>
  ) : (
    <Status eyes="wide" tone="warn">
      {launchAction.label}
    </Status>
  );

  const onPlay = (): void => {
    selectInstance(inst.id);
    void launchGame();
  };
  const onInstall = (): void => {
    if (launchAction.state_id === 'setup_pending') {
      void resumeInstanceSetup(inst.id);
      return;
    }
    if (!installStatus.item) return;
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
  const packs = cachedModProvenance(inst.id)?.packs ?? [];
  const loader = inst.version_display;

  return (
    <div class="cp-view-page cp-instance" data-running={playing}>
      <Vista seed={artSeedFor(inst)} version={inst.version_id} installing={installStatus.installing} />
      <header class="cp-instance-head">
        <h1 class="cp-instance-name" title={inst.name}>
          {inst.name}
        </h1>
        <div class="cp-chips">
          {status}
          <Chip>
            {loader.loader_label}
            {loader.loader_version_label ? ` ${loader.loader_version_label}` : ''}
          </Chip>
          <Chip mono>{loader.minecraft_label}</Chip>
          {inst.last_played_at && (
            <Chip icon="clock" title={`Last played ${fmtRelative(inst.last_played_at)}`}>
              {fmtRelative(inst.last_played_at)}
            </Chip>
          )}
          <PackChips packs={packs} instanceId={inst.id} />
        </div>
        <div class="cp-instance-actions">
          {!sessionActive ? (
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
          ) : (
            canStop && (
              <Button size="lg" variant="secondary" icon="stop" disabled={session?.stopping} onClick={onStop}>
                Stop
              </Button>
            )
          )}
          <IconButton
            icon="dots"
            size={36}
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
        <Tabs
          tabs={visibleTabs.map((tab) => ({ ...tab, count: tabCount(tab.id) }))}
          active={activeTab}
          onSelect={selectTab}
        />
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
          onRetryInstall={() => matchingInstallFailure && retryFailedInstall(matchingInstallFailure)}
        />
      )}
      {!installLocked && activeTab === 'mods' && (
        <ModsPane inst={inst} resources={resources} onRefresh={reloadResources} />
      )}
      {!installLocked && (activeTab === 'resourcepacks' || activeTab === 'shaderpacks') && (
        <PacksPane key={activeTab} inst={inst} folder={activeTab} />
      )}
      {!installLocked && activeTab === 'worlds' && (
        <WorldsPane inst={inst} resources={resources} onRefresh={reloadResources} />
      )}
      {!installLocked && activeTab === 'screenshots' && (
        <ScreenshotsPane inst={inst} resources={resources} onRefresh={reloadResources} />
      )}
      {!installLocked && activeTab === 'logs' && (
        <LogsPane inst={inst} resources={resources} processLive={processLive} onRefresh={reloadResources} />
      )}
      {!installLocked && activeTab === 'settings' && <SettingsPane inst={inst} />}
    </div>
  );
}
