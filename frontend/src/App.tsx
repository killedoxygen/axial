import { h } from 'preact';
import type { ComponentType, JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { AppFrame } from './shell/AppFrame';
import { HomeView } from './views/home/HomeView';
import { InstancesView } from './views/instances/InstancesView';
import { DialogHost } from './ui/Dialog';
import { SetupOverlay } from './views/setup/SetupOverlay';
import { ContextMenuHost } from './ui/ContextMenu';
import { ToastHost } from './ui/ToastHost';
import { CommandPalette } from './ui/CommandPalette';
import { route, showOnboardingOverlay, showSetupOverlay } from './ui-state';
import { bootstrapError, bootstrapState, devMode } from './store';
import { useShortcuts } from './hooks/use-shortcuts';

type DevLabViewComponent = typeof import('./views/dev-lab/DevLabView')['DevLabView'];

const InstanceDetailRoute = createRouteLoader<{ id: string }>(
  async () => (await import('./views/instance/InstanceDetailView')).InstanceDetailView,
);

const CreateRoute = createRouteLoader(
  async () => (await import('./views/create/CreateView')).CreateView,
);

const AccountsRoute = createRouteLoader(
  async () => (await import('./views/accounts/AccountsView')).AccountsView,
);

const SettingsRoute = createRouteLoader(
  async () => (await import('./views/settings/SettingsView')).SettingsView,
);

const DownloadsRoute = createRouteLoader(
  async () => (await import('./views/downloads/DownloadsView')).DownloadsView,
);

const OnboardingOverlay = createRouteLoader(
  async () => (await import('./views/onboarding/Onboarding')).Onboarding,
);

const loadDevLabView = __CROOPOR_ENABLE_DEV_LAB__
  ? async (): Promise<DevLabViewComponent> => (await import('./views/dev-lab/DevLabView')).DevLabView
  : null;

function createRouteLoader<P extends object>(load: () => Promise<ComponentType<P>>): ComponentType<P> {
  return function LazyRouteView(props: P): JSX.Element {
    const [View, setView] = useState<ComponentType<P> | null>(null);
    const [failed, setFailed] = useState(false);

    useEffect(() => {
      let mounted = true;
      setFailed(false);
      void load()
        .then((view) => {
          if (mounted) setView(() => view);
        })
        .catch(() => {
          if (mounted) setFailed(true);
        });
      return () => { mounted = false; };
    }, []);

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

function BootState(): JSX.Element | null {
  const s = bootstrapState.value;
  if (s === 'ready') return null;
  const err = bootstrapError.value;
  return (
    <div style={{
      position: 'fixed', inset: 0,
      display: 'flex', alignItems: 'center', justifyContent: 'center',
      background: 'var(--bg)', color: 'var(--text)',
      zIndex: 2000,
      fontFamily: 'inherit',
    }}>
      <div style={{
        padding: 24, borderRadius: 'var(--r-lg)',
        background: 'var(--surface)', border: '1px solid var(--line)',
        maxWidth: 440, textAlign: 'center',
      }}>
        {s === 'loading' && <div style={{ fontSize: 14, color: 'var(--text-dim)' }}>Starting Croopor…</div>}
        {s === 'error' && (
          <>
            <div style={{ fontSize: 16, fontWeight: 600, marginBottom: 6 }}>Failed to connect</div>
            <div style={{ fontSize: 13, color: 'var(--text-dim)' }}>{err || 'The launcher could not load its initial state.'}</div>
          </>
        )}
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
    return () => { mounted = false; };
  }, [load]);

  return DevLabView ? <DevLabView /> : <SettingsRoute />;
}

function CurrentView(): JSX.Element {
  const r = route.value;
  switch (r.name) {
    case 'home': return <HomeView />;
    case 'instances': return <InstancesView />;
    case 'instance': return <InstanceDetailRoute id={r.id} />;
    case 'create': return <CreateRoute />;
    case 'dev-lab': return <DevLabRoute />;
    case 'downloads': return <DownloadsRoute />;
    case 'accounts': return <AccountsRoute />;
    case 'settings': return <SettingsRoute />;
  }
}

export function App(): JSX.Element {
  useShortcuts();
  return (
    <>
      <AppFrame><CurrentView /></AppFrame>
      {showSetupOverlay.value && <SetupOverlay />}
      {showOnboardingOverlay.value && <OnboardingOverlay />}
      <DialogHost />
      <ContextMenuHost />
      <ToastHost />
      <CommandPalette />
      <BootState />
    </>
  );
}
