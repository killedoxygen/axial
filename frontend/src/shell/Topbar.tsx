import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Icon } from '../ui/Icons';
import { IconButton } from '../ui/Atoms';
import { PlayerHeadPreview } from '../ui/PlayerHeadPreview';
import { WindowControls } from './WindowControls';
import { MusicWidget } from './MusicWidget';
import { api, apiResourceUrl } from '../api';
import { goBack, goForward, navigate, route } from '../ui-state';
import { runningSessions, instances, launchState, installState, config } from '../store';
import { windowStartDragging, windowToggleMaximize, hasNativeDesktopRuntime } from '../native';
import { launchStageViewFrom } from '../launch-stages';

interface SkinProfile {
  username: string;
  texture_url: string | null;
  head_url: string | null;
}

function assertUnreachable(value: never): never {
  throw new Error(`Unhandled route: ${JSON.stringify(value)}`);
}

function isSkinProfile(value: unknown): value is SkinProfile {
  if (!value || typeof value !== 'object') return false;
  const record = value as Record<string, unknown>;
  return (
    typeof record.username === 'string' &&
    (typeof record.texture_url === 'string' || record.texture_url === null) &&
    (typeof record.head_url === 'string' || record.head_url === null)
  );
}

function crumbsFor(): { label: string; onClick?: () => void }[] {
  const r = route.value;
  switch (r.name) {
    case 'home': return [{ label: 'Home' }];
    case 'instances': return [{ label: 'Instances' }];
    case 'instance': {
      const inst = instances.value.find(i => i.id === r.id);
      return [
        { label: 'Instances', onClick: () => navigate({ name: 'instances' }) },
        { label: inst?.name || 'Instance' },
      ];
    }
    case 'create': return [
      { label: 'Instances', onClick: () => navigate({ name: 'instances' }) },
      { label: 'New' },
    ];
    case 'dev-lab': return [
      { label: 'Settings', onClick: () => navigate({ name: 'settings' }) },
      { label: 'Dev lab' },
    ];
    case 'downloads': return [{ label: 'Downloads' }];
    case 'accounts': return [{ label: 'Accounts & skins' }];
    case 'settings': return [{ label: 'Settings' }];
    default: return assertUnreachable(r);
  }
}

// Topbar status pill
// Priority: running instance > active install > launch preparing > idle
function StatusPill(): JSX.Element {
  const sessions = runningSessions.value;
  const runIds = Object.keys(sessions);
  const inst = runIds.length > 0 ? instances.value.find(i => i.id === runIds[0]) : null;
  const session = runIds.length > 0 ? sessions[runIds[0]] : null;

  if (inst && session) {
    const label = session.stopping ? 'Stopping' : launchStageViewFrom(session.state)?.label || 'Playing';
    return (
      <button
        class="cp-status-pill cp-status-pill--running cp-nodrag"
        onClick={() => navigate({ name: 'instance', id: inst.id })}
        title="Jump to running instance"
      >
        <span class="cp-status-dot" />
        <span class="cp-status-pill-label">{label} · {inst.name}</span>
      </button>
    );
  }

  const install = installState.value;
  if (install.status === 'active') {
    return (
      <button
        class="cp-status-pill cp-status-pill--running cp-nodrag"
        onClick={() => navigate({ name: 'downloads' })}
        title={`${install.label} · ${Math.round(install.pct)}%`}
      >
        <span class="cp-status-dot" />
        <span class="cp-status-pill-label">{install.label} · {Math.round(install.pct)}%</span>
      </button>
    );
  }

  const launch = launchState.value;
  if (launch.status === 'preparing') {
    const li = instances.value.find(i => i.id === launch.instanceId);
    return (
      <span class="cp-status-pill cp-status-pill--running cp-nodrag" title={`${launch.label} · ${li?.name || 'launch'}`}>
        <span class="cp-status-dot" />
        <span class="cp-status-pill-label">{launch.label} · {li?.name || 'launch'}</span>
      </span>
    );
  }

  return (
    <span class="cp-status-pill cp-nodrag">
      <span class="cp-status-dot" />
      <span class="cp-status-pill-label">idle</span>
    </span>
  );
}

function AccountChip(): JSX.Element {
  const cfg = config.value;
  const configuredUsername = (cfg?.username || 'Player').slice(0, 24);
  const routeKey = JSON.stringify(route.value);
  const [profile, setProfile] = useState<SkinProfile | null>(null);

  useEffect(() => {
    let active = true;
    setProfile(null);

    if (!cfg) {
      return () => {
        active = false;
      };
    }

    void api('GET', '/skin/profile')
      .then((res: unknown) => {
        if (!active) return;
        if (isSkinProfile(res)) setProfile(res);
      })
      .catch(() => {
        if (active) setProfile(null);
      });

    return () => {
      active = false;
    };
  }, [cfg?.username, routeKey]);

  const textureSrc = profile?.texture_url ? apiResourceUrl(profile.texture_url) : undefined;
  const headSrc = profile?.head_url ? apiResourceUrl(profile.head_url) : undefined;
  const displayUsername = (profile?.username || configuredUsername).slice(0, 24);
  const label = `Open Accounts & skins for ${displayUsername}`;

  return (
    <button
      type="button"
      class="cp-account-chip cp-nodrag"
      onClick={() => navigate({ name: 'accounts' })}
      aria-label={label}
      title={label}
    >
      <PlayerHeadPreview
        username={displayUsername}
        src={headSrc}
        textureSrc={textureSrc}
        size={24}
        radius={7}
      />
      <span class="cp-account-chip-name">{displayUsername}</span>
    </button>
  );
}

export function Topbar(): JSX.Element {
  const [isNative] = useState(hasNativeDesktopRuntime());

  const onDragAreaDoubleClick = (e: MouseEvent): void => {
    if (!isNative) return;
    if ((e.target as HTMLElement)?.closest('.cp-nodrag')) return;
    void windowToggleMaximize();
  };

  const onDragAreaMouseDown = (e: MouseEvent): void => {
    if (!isNative) return;
    if ((e.target as HTMLElement)?.closest('.cp-nodrag')) return;
    if (e.button !== 0) return;
    void windowStartDragging();
  };

  const crumbs = crumbsFor();
  return (
    <div
      class="cp-topbar cp-drag"
      onMouseDown={onDragAreaMouseDown}
      onDblClick={onDragAreaDoubleClick}
    >
      <div class="cp-nodrag" style={{ display: 'flex', alignItems: 'center', gap: 2 }}>
        <IconButton icon="arrow-left" size={28} tooltip="Back" onClick={goBack} />
        <IconButton icon="arrow-right" size={28} tooltip="Forward" onClick={goForward} />
      </div>
      <div class="cp-topbar-crumbs cp-nodrag">
        {crumbs.map((c, i) => (
          <div key={i} style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}>
            {i > 0 && <Icon name="chevron-right" size={12} color="var(--text-mute)" />}
            <button
              class={`cp-topbar-crumb${i === crumbs.length - 1 ? ' cp-topbar-crumb--last' : ''}`}
              onClick={c.onClick}
              disabled={!c.onClick}
              style={{
                background: 'none',
                border: 'none',
                color: 'inherit',
                font: 'inherit',
                padding: 0,
                cursor: c.onClick ? 'pointer' : 'default',
              }}
            >{c.label}</button>
          </div>
        ))}
      </div>
      <div class="cp-topbar-spacer" />
      <div class="cp-topbar-actions cp-nodrag">
        <StatusPill />
        <AccountChip />
        <MusicWidget />
      </div>
      <WindowControls />
    </div>
  );
}
