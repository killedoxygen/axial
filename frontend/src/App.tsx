import type { JSX } from 'preact';
import { AppFrame } from './shell/AppFrame';
import { HomeView } from './views/home/HomeView';
import { InstancesView } from './views/instances/InstancesView';
import { InstanceDetailView } from './views/instance/InstanceDetailView';
import { CreateView } from './views/create/CreateView';
import { BrowseView } from './views/browse/BrowseView';
import { DownloadsView } from './views/downloads/DownloadsView';
import { AccountsView } from './views/accounts/AccountsView';
import { SettingsView } from './views/settings/SettingsView';
import { Onboarding } from './views/onboarding/Onboarding';
import { DialogHost } from './ui/Dialog';
import { SetupOverlay } from './views/setup/SetupOverlay';
import { ContextMenuHost } from './ui/ContextMenu';
import { ToastHost } from './ui/ToastHost';
import { route, showOnboardingOverlay, showSetupOverlay } from './ui-state';
import { bootstrapError, bootstrapState } from './store';
import { useShortcuts } from './hooks/use-shortcuts';
import './views/views.css';

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

function CurrentView(): JSX.Element {
  const r = route.value;
  switch (r.name) {
    case 'home': return <HomeView />;
    case 'instances': return <InstancesView />;
    case 'instance': return <InstanceDetailView id={r.id} />;
    case 'create': return <CreateView />;
    case 'browse': return <BrowseView />;
    case 'downloads': return <DownloadsView />;
    case 'accounts': return <AccountsView />;
    case 'settings': return <SettingsView />;
  }
}

export function App(): JSX.Element {
  useShortcuts();
  return (
    <>
      <AppFrame><CurrentView /></AppFrame>
      {showSetupOverlay.value && <SetupOverlay />}
      {showOnboardingOverlay.value && <Onboarding />}
      <DialogHost />
      <ContextMenuHost />
      <ToastHost />
      <BootState />
    </>
  );
}
