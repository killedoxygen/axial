import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Icon } from '../ui/Icons';
import { IconButton } from '../ui/Atoms';
import { WindowControls } from './WindowControls';
import { MusicWidget } from './MusicWidget';
import { route, navigate, windowMaximized } from '../ui-state';
import { runningSessions, instances, launchState, installState } from '../store';
import { windowStartDragging, hasNativeDesktopRuntime } from '../native';

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
    case 'browse': return [{ label: 'Browse' }];
    case 'downloads': return [{ label: 'Downloads' }];
    case 'accounts': return [{ label: 'Accounts & skins' }];
    case 'settings': return [{ label: 'Settings' }];
  }
}

// Topbar status pill
// Priority: running instance > active install > launch preparing > idle
function StatusPill(): JSX.Element {
  const sessions = runningSessions.value;
  const runIds = Object.keys(sessions);
  const inst = runIds.length > 0 ? instances.value.find(i => i.id === runIds[0]) : null;

  if (inst) {
    return (
      <button
        class="cp-status-pill cp-status-pill--running cp-nodrag"
        onClick={() => navigate({ name: 'instance', id: inst.id })}
        title="Jump to running instance"
      >
        <span class="cp-status-dot" />
        Playing · {inst.name}
      </button>
    );
  }

  const install = installState.value;
  if (install.status === 'active') {
    return (
      <button
        class="cp-status-pill cp-status-pill--running cp-nodrag"
        onClick={() => navigate({ name: 'downloads' })}
      >
        <span class="cp-status-dot" />
        {install.label} · {Math.round(install.pct)}%
      </button>
    );
  }

  const launch = launchState.value;
  if (launch.status === 'preparing') {
    const li = instances.value.find(i => i.id === launch.instanceId);
    return (
      <span class="cp-status-pill cp-status-pill--running cp-nodrag">
        <span class="cp-status-dot" />
        Preparing {li?.name || 'launch'}…
      </span>
    );
  }

  return (
    <span class="cp-status-pill cp-nodrag">
      <span class="cp-status-dot" />
      idle
    </span>
  );
}

export function Topbar(): JSX.Element {
  const [isNative] = useState(hasNativeDesktopRuntime());

  const onDragAreaDoubleClick = (): void => {
    if (!isNative) return;
  };

  const onDragAreaMouseDown = (e: MouseEvent): void => {
    if (!isNative) return;
    if ((e.target as HTMLElement)?.closest('.cp-nodrag')) return;
    if (e.button !== 0) return;
    void windowStartDragging();
  };

  useEffect(() => {
    windowMaximized.value = false;
  }, []);

  const crumbs = crumbsFor();
  return (
    <div
      class="cp-topbar cp-drag"
      onMouseDown={onDragAreaMouseDown}
      onDblClick={onDragAreaDoubleClick}
    >
      <div class="cp-nodrag" style={{ display: 'flex', alignItems: 'center', gap: 2 }}>
        <IconButton icon="arrow-left" size={28} tooltip="Back" onClick={() => history.back()} />
        <IconButton icon="arrow-right" size={28} tooltip="Forward" onClick={() => history.forward()} />
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
      <div class="cp-nodrag" style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}>
        <StatusPill />
        <MusicWidget />
      </div>
      <WindowControls />
    </div>
  );
}
