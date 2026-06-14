import { h } from 'preact';
import type { ComponentType, JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { AppFrame } from './shell/AppFrame';
import { BootSplash } from './shell/BootSplash';
import { HomeView } from './views/home/HomeView';
import { DialogHost } from './ui/Dialog';
import { ContextMenuHost } from './ui/ContextMenu';
import { ToastHost } from './ui/ToastHost';
import { Logo } from './ui/Logo';
import { commandPaletteOpen, createOpen, route, showOnboardingOverlay, showSetupOverlay } from './ui-state';
import { devMode } from './store';
import { useShortcuts } from './hooks/use-shortcuts';

type DevLabViewComponent = (typeof import('./views/dev-lab/DevLabView'))['DevLabView'];
type CommandPaletteComponent = (typeof import('./ui/CommandPalette'))['CommandPalette'];
type SetupOverlayComponent = (typeof import('./views/setup/SetupOverlay'))['SetupOverlay'];

let loadedCommandPalette: CommandPaletteComponent | null = null;
let loadedSetupOverlay: SetupOverlayComponent | null = null;

const InstanceDetailRoute = createRouteLoader<{ id: string }>(
  async () => (await import('./views/instance/InstanceDetailView')).InstanceDetailView,
);

const InstancesRoute = createRouteLoader(async () => (await import('./views/instances/InstancesView')).InstancesView);

const CreateOverlay = createRouteLoader(async () => (await import('./views/create/CreateView')).CreateView);

const AccountsRoute = createRouteLoader(async () => (await import('./views/accounts/AccountsView')).AccountsView);

const SettingsRoute = createRouteLoader(async () => (await import('./views/settings/SettingsView')).SettingsView);

const DownloadsRoute = createRouteLoader(async () => (await import('./views/downloads/DownloadsView')).DownloadsView);

const OnboardingOverlay = createRouteLoader(async () => (await import('./views/onboarding/Onboarding')).Onboarding);

const loadDevLabView = __CROOPOR_ENABLE_DEV_LAB__
  ? async (): Promise<DevLabViewComponent> => (await import('./views/dev-lab/DevLabView')).DevLabView
  : null;

function createRouteLoader<P extends object>(load: () => Promise<ComponentType<P>>): ComponentType<P> {
  let loadedView: ComponentType<P> | null = null;

  return function LazyRouteView(props: P): JSX.Element {
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

    return View ? h(View, props) : <RouteLoadingFallback failed={failed} />;
  };
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

function SetupLoadingFallback({ failed = false }: { failed?: boolean }): JSX.Element {
  return (
    <div class="cp-setup-overlay" role="status" aria-live="polite">
      <div class="cp-setup-card">
        <Logo className="cp-logo" size={48} />
        <h1 class="cp-setup-title">{failed ? 'Could not load setup' : 'Loading setup...'}</h1>
        <p class="cp-setup-sub">
          {failed ? 'Restart the launcher and try again.' : 'Preparing the library setup flow.'}
        </p>
        {!failed && <div class="cp-setup-progress" />}
      </div>
    </div>
  );
}

function DevLabRoute(): JSX.Element {
  if (!loadDevLabView || !devMode.value) return <SettingsRoute />;
  return <DevLabLoader load={loadDevLabView} />;
}

function DevLabLoader({ load }: { load: () => Promise<DevLabViewComponent> }): JSX.Element {
  const [DevLabView, setDevLabView] = useState<DevLabViewComponent | null>(null);

  useEffect(() => {
    let mounted = true;
    void load().then((view) => {
      if (mounted) setDevLabView(() => view);
    });
    return () => {
      mounted = false;
    };
  }, [load]);

  return DevLabView ? <DevLabView /> : <SettingsRoute />;
}

function LazyCommandPalette(): JSX.Element | null {
  const [CommandPaletteView, setCommandPaletteView] = useState<CommandPaletteComponent | null>(
    () => loadedCommandPalette,
  );

  useEffect(() => {
    if (CommandPaletteView) return;
    let mounted = true;
    void import('./ui/CommandPalette').then((module) => {
      loadedCommandPalette = module.CommandPalette;
      if (mounted) setCommandPaletteView(() => module.CommandPalette);
    });
    return () => {
      mounted = false;
    };
  }, [CommandPaletteView]);

  return CommandPaletteView ? <CommandPaletteView /> : null;
}

function LazySetupOverlay(): JSX.Element {
  const [SetupOverlayView, setSetupOverlayView] = useState<SetupOverlayComponent | null>(loadedSetupOverlay);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    if (SetupOverlayView) return;
    let mounted = true;
    setFailed(false);
    void import('./views/setup/SetupOverlay')
      .then((module) => {
        loadedSetupOverlay = module.SetupOverlay;
        if (mounted) setSetupOverlayView(() => module.SetupOverlay);
      })
      .catch(() => {
        if (mounted) setFailed(true);
      });
    return () => {
      mounted = false;
    };
  }, [SetupOverlayView]);

  return SetupOverlayView ? <SetupOverlayView /> : <SetupLoadingFallback failed={failed} />;
}

function CurrentView(): JSX.Element {
  const r = route.value;
  switch (r.name) {
    case 'home':
      return <HomeView />;
    case 'instances':
      return <InstancesRoute />;
    case 'instance':
      return <InstanceDetailRoute id={r.id} />;
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

export function App(): JSX.Element {
  useShortcuts();
  return (
    <>
      <AppFrame>
        <CurrentView />
      </AppFrame>
      {createOpen.value && <CreateOverlay />}
      {showSetupOverlay.value && <LazySetupOverlay />}
      {showOnboardingOverlay.value && <OnboardingOverlay />}
      <DialogHost />
      <ContextMenuHost />
      <ToastHost />
      {commandPaletteOpen.value && <LazyCommandPalette />}
      <BootSplash />
    </>
  );
}
