import { h } from 'preact';
import type { ComponentChildren, ComponentType, JSX } from 'preact';
import { useEffect, useErrorBoundary, useLayoutEffect, useState } from 'preact/hooks';
import { AppFrame } from './shell/AppFrame';
import { BootSplash } from './shell/BootSplash';
import { HomeView } from './views/home/HomeView';
import { Button, Empty } from './ui/Atoms';
import { DialogHost } from './ui/Dialog';
import { ContextMenuHost } from './ui/ContextMenu';
import { TipHost } from './ui/Tip';
import { ToastHost } from './ui/ToastHost';
import {
  accountSwitcherOpen,
  commandPaletteOpen,
  createOpen,
  prepareViewScroll,
  restoreViewScroll,
  route,
  routeScrollKey,
  showOnboardingOverlay,
} from './ui-state';
import { devMode } from './store';
import { useLeave } from './hooks/use-leave';
import { useShortcuts } from './hooks/use-shortcuts';
import { leaveView } from './ui/motion';
import { reportRenderError } from './error-reporting';
import { reloadApplication } from './preferences/persistence';

type DevLabViewComponent = (typeof import('./views/dev-lab/DevLabView'))['DevLabView'];

const deferredViewPreloads: (() => Promise<void>)[] = [];

/* Warm every lazily loaded view during idle time so the first navigation (or
   first New Instance) renders synchronously instead of hitting a chunk load.
   Failures are ignored: the lazy path retries on actual navigation. */
export function preloadDeferredViews(): void {
  for (const preload of deferredViewPreloads) {
    void preload().catch(() => undefined);
  }
}

const InstanceDetailRoute = createRouteLoader<{ id: string }>(
  async () => (await import('./views/instance/InstanceDetailView')).InstanceDetailView,
);

const InstancesRoute = createRouteLoader(async () => (await import('./views/instances/InstancesView')).InstancesView);

const DiscoverRoute = createPageRouteLoader(async () => (await import('./views/discover/DiscoverView')).DiscoverView);
const ContentDetailRoute = createRouteLoader(
  async () => (await import('./views/discover/ContentDetailView')).ContentDetailView,
);

const CreateOverlay = createRouteLoader(async () => (await import('./views/create/CreateView')).CreateView);
const CommandPaletteOverlay = createOverlayLoader(async () => (await import('./ui/CommandPalette')).CommandPalette);
const AccountSwitcherOverlay = createOverlayLoader(
  async () => (await import('./views/accounts/AccountSwitcher')).AccountSwitcher,
);

const AccountsRoute = createRouteLoader(async () => (await import('./views/accounts/AccountsView')).AccountsView);

const SettingsRoute = createRouteLoader(async () => (await import('./views/settings/SettingsView')).SettingsView);

const DownloadsRoute = createPageRouteLoader(
  async () => (await import('./views/downloads/DownloadsView')).DownloadsView,
);

const OnboardingOverlay = createRouteLoader(async () => (await import('./views/onboarding/Onboarding')).Onboarding);

const loadDevLabView = __AXIAL_ENABLE_DEV_LAB__
  ? async (): Promise<DevLabViewComponent> => (await import('./views/dev-lab/DevLabView')).DevLabView
  : null;

const DevLabRouteView = loadDevLabView ? createRouteLoader(loadDevLabView) : null;

function createPageRouteLoader<P extends object>(load: () => Promise<ComponentType<P>>): ComponentType<P> {
  return createRouteLoader(load, { restoresViewScroll: true });
}

/** An overlay shows nothing until its code arrives; a route says it is loading. */
function createOverlayLoader<P extends object>(load: () => Promise<ComponentType<P>>): ComponentType<P> {
  return createRouteLoader(load, { silent: true });
}

function createRouteLoader<P extends object>(
  load: () => Promise<ComponentType<P>>,
  { restoresViewScroll = false, silent = false } = {},
): ComponentType<P> {
  let loadedView: ComponentType<P> | null = null;

  deferredViewPreloads.push(async () => {
    loadedView ??= await load();
  });

  return function LazyRouteView(props: P): JSX.Element | null {
    const [View, setView] = useState<ComponentType<P> | null>(() => loadedView);
    const [failed, setFailed] = useState(false);

    useEffect(() => {
      if (View) return;
      let mounted = true;
      setFailed(false);
      void load()
        .then((view) => {
          loadedView = view;
          if (mounted) setView(() => view);
        })
        .catch(() => {
          if (mounted) setFailed(true);
        });
      return () => {
        mounted = false;
      };
    }, [View]);

    if (!View) return silent ? null : <RouteLoadingFallback failed={failed} />;
    const content = h(View, props);
    return restoresViewScroll ? <RouteScrollReady>{content}</RouteScrollReady> : content;
  };
}

function RouteScrollReady({ children }: { children: ComponentChildren }): JSX.Element {
  const key = routeScrollKey(route.value);
  useLayoutEffect(() => restoreViewScroll(key), [key]);
  return <>{children}</>;
}

function RouteLoadingFallback({ failed = false }: { failed?: boolean }): JSX.Element {
  return (
    <div
      role="status"
      aria-live="polite"
      style={{
        minHeight: 'min(420px, 64vh)',
        display: 'grid',
        placeItems: 'center',
        color: 'var(--text-dim)',
        fontSize: 13,
      }}
    >
      {failed ? 'Could not load view.' : 'Loading view...'}
    </div>
  );
}

function DevLabRoute(): JSX.Element {
  if (!DevLabRouteView || !devMode.value) return <SettingsRoute />;
  return <DevLabRouteView />;
}

function CurrentView(): JSX.Element {
  const r = route.value;
  const routeKey = routeScrollKey(r);
  useLayoutEffect(() => {
    prepareViewScroll(routeKey);
  }, [routeKey]);
  switch (r.name) {
    case 'home':
      return (
        <RouteScrollReady>
          <HomeView />
        </RouteScrollReady>
      );
    case 'instances':
      return <InstancesRoute />;
    case 'instance':
      return <InstanceDetailRoute id={r.id} />;
    case 'discover':
      return <DiscoverRoute />;
    case 'content':
      return <ContentDetailRoute />;
    case 'dev-lab':
      return <DevLabRoute />;
    case 'downloads':
      return <DownloadsRoute />;
    case 'accounts':
      return <AccountsRoute />;
    case 'settings':
      return <SettingsRoute />;
  }
}

/** One place's page. Going to another place, it leaves over the view while the next arrives. */
function RoutePage(): JSX.Element {
  const leaving = useLeave<HTMLDivElement>(leaveView);
  return (
    <div class="cp-route" ref={leaving}>
      <CurrentView />
    </div>
  );
}

function AppErrorBoundary({ children }: { children: ComponentChildren }): JSX.Element {
  const [reloadFailed, setReloadFailed] = useState(false);
  const [error] = useErrorBoundary((caughtError) => {
    reportRenderError(caughtError);
  });

  if (error) {
    const reload = (): void => {
      void (async () => {
        try {
          setReloadFailed(!(await reloadApplication()));
        } catch {
          setReloadFailed(true);
        }
      })();
    };
    return (
      <div class="cp-crash">
        <Empty
          eyes="x"
          title="Axial hit a render error"
          actions={
            <Button variant="secondary" icon="refresh" onClick={reload}>
              Reload
            </Button>
          }
        >
          {reloadFailed
            ? 'Interface preferences could not be confirmed. Keep this window open and retry Reload.'
            : 'Reloading usually gets the launcher back in sync with the backend.'}
        </Empty>
      </div>
    );
  }

  return <>{children}</>;
}

function AppContent(): JSX.Element {
  useShortcuts();
  return (
    <>
      <AppFrame>
        <RoutePage key={route.value.name} />
      </AppFrame>
      {createOpen.value && <CreateOverlay />}
      {accountSwitcherOpen.value && <AccountSwitcherOverlay />}
      {showOnboardingOverlay.value && <OnboardingOverlay />}
      <DialogHost />
      <ContextMenuHost />
      <ToastHost />
      <TipHost />
      {commandPaletteOpen.value && <CommandPaletteOverlay />}
      <BootSplash />
    </>
  );
}

export function App(): JSX.Element {
  return (
    <AppErrorBoundary>
      <AppContent />
    </AppErrorBoundary>
  );
}
