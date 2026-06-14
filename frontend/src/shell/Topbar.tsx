import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Icon } from '../ui/Icons';
import { IconButton } from '../ui/Atoms';
import { WindowControls } from './WindowControls';
import { MusicWidget } from './MusicWidget';
import { goBack, goForward, navigate, route } from '../ui-state';
import {
  runningSessions,
  instances,
  versionById,
  launchState,
  installState,
  installQueue,
  installFailure,
} from '../store';
import { minecraftVersionLabel } from '../version-display';
import { windowStartDragging, windowToggleMaximize, hasNativeDesktopRuntime } from '../native';
import { launchStageViewFrom } from '../launch-stages';
import { formatInstallItemLabel } from '../install-labels';
import { countDownRemainingSeconds, formatRemainingTime } from '../progress-estimation';

function assertUnreachable(value: never): never {
  throw new Error(`Unhandled route: ${JSON.stringify(value)}`);
}

function crumbsFor(): { label: string; onClick?: () => void }[] {
  const r = route.value;
  switch (r.name) {
    case 'home':
      return [{ label: 'Home' }];
    case 'instances':
      return [{ label: 'Instances' }];
    case 'instance': {
      const inst = instances.value.find((i) => i.id === r.id);
      return [
        { label: 'Instances', onClick: () => navigate({ name: 'instances' }) },
        { label: inst?.name || 'Instance' },
      ];
    }
    case 'dev-lab':
      return [{ label: 'Settings', onClick: () => navigate({ name: 'settings' }) }, { label: 'Dev lab' }];
    case 'downloads':
      return [{ label: 'Downloads' }];
    case 'accounts':
      return [{ label: 'Accounts & skins' }];
    case 'settings':
      return [{ label: 'Settings' }];
    default:
      return assertUnreachable(r);
  }
}

function versionTag(versionId: string | undefined): string | null {
  return minecraftVersionLabel(versionById(versionId), '') || null;
}

function StatusMark({ icon }: { icon: string }): JSX.Element {
  return (
    <span class="cp-status-mark" aria-hidden="true">
      <Icon name={icon} size={14} stroke={2.1} />
    </span>
  );
}

function StatusPill(): JSX.Element {
  const sessions = runningSessions.value;
  const install = installState.value;
  const [etaNow, setEtaNow] = useState(() => Date.now());

  useEffect(() => {
    if (install.status !== 'active' || !install.remainingSeconds) return;
    setEtaNow(Date.now());
    const intervalId = window.setInterval(() => {
      setEtaNow(Date.now());
    }, 1000);
    return () => {
      window.clearInterval(intervalId);
    };
  }, [
    install.status,
    install.status === 'active' ? install.remainingSeconds : undefined,
    install.status === 'active' ? install.remainingSecondsUpdatedAt : undefined,
  ]);

  const runIds = Object.keys(sessions);
  const inst = runIds.length > 0 ? instances.value.find((i) => i.id === runIds[0]) : null;
  const session = runIds.length > 0 ? sessions[runIds[0]] : null;

  if (inst && session) {
    const label = session.stopping ? 'Stopping' : launchStageViewFrom(session.state)?.label || 'Playing';
    const tag = versionTag(inst.version_id);
    return (
      <button
        class="cp-status-pill cp-status-pill--running cp-nodrag"
        onClick={() => navigate({ name: 'instance', id: inst.id })}
        title={`${label} · ${inst.name}`}
        aria-label={`Open running instance. ${label} · ${inst.name}`}
      >
        <StatusMark icon="play" />
        <span class="cp-status-pill-label">{label}</span>
        {tag && <span class="cp-status-pill-meta">{tag}</span>}
      </button>
    );
  }

  if (install.status === 'active') {
    const queuedCount = installQueue.value.length;
    const queuedLabel = queuedCount > 0 ? ` · ${queuedCount} queued` : '';
    const installPct = Math.round(Math.max(0, Math.min(100, install.pct)));
    const installPhase = install.phase ? ` · ${install.phase.replace(/_/g, ' ')}` : '';
    const installRemainingSeconds = countDownRemainingSeconds(
      install.remainingSeconds,
      install.remainingSecondsUpdatedAt,
      etaNow,
    );
    const installEta = installRemainingSeconds ? formatRemainingTime(installRemainingSeconds) : null;
    const installName = install.displayName || install.versionId;
    const installTag = install.item.loader?.minecraftVersion || versionTag(install.versionId);
    const installTitle = `${installName}: ${install.label} · ${installPct}%${installEta ? ` · ${installEta} left` : ''}${queuedLabel}${installPhase}`;
    const installStyle = { '--cp-install-ratio': String(installPct / 100) } as JSX.CSSProperties;

    return (
      <button
        class="cp-status-pill cp-status-pill--installing cp-nodrag"
        onClick={() => navigate({ name: 'downloads' })}
        title={installTitle}
        aria-label={`Open downloads. ${installTitle}`}
        style={installStyle}
      >
        <StatusMark icon="download" />
        {installTag && <span class="cp-status-pill-meta">{installTag}</span>}
        <span class="cp-status-pill-pct">{installPct}%</span>
        {installEta && <span class="cp-status-pill-eta">{installEta}</span>}
        {queuedCount > 0 && <span class="cp-status-pill-chip">+{queuedCount}</span>}
      </button>
    );
  }

  const launch = launchState.value;
  if (launch.status === 'preparing') {
    const li = instances.value.find((i) => i.id === launch.instanceId);
    const prepTag = versionTag(li?.version_id);
    return (
      <span
        class="cp-status-pill cp-status-pill--preparing cp-nodrag"
        title={`${launch.label} · ${li?.name || 'launch'}`}
      >
        <StatusMark icon="clock" />
        <span class="cp-status-pill-label">{launch.label}</span>
        {prepTag && <span class="cp-status-pill-meta">{prepTag}</span>}
      </span>
    );
  }

  const queued = installQueue.value;
  if (queued.length > 0) {
    const firstQueued = queued[0];
    const queuedLabel = queued.length === 1 ? '1 queued' : `${queued.length} queued`;
    const queuedTitle = `${queuedLabel}. Next: ${formatInstallItemLabel(firstQueued)}`;
    return (
      <button
        class="cp-status-pill cp-status-pill--queued cp-nodrag"
        onClick={() => navigate({ name: 'downloads' })}
        title={queuedTitle}
        aria-label={`Open downloads. ${queuedTitle}`}
      >
        <StatusMark icon="archive" />
        <span class="cp-status-pill-label">Queued</span>
        {queued.length > 1 && <span class="cp-status-pill-chip">{queued.length}</span>}
      </button>
    );
  }

  const failure = installFailure.value;
  if (failure) {
    const title = `${failure.displayName}: ${failure.message}`;
    return (
      <button
        class="cp-status-pill cp-status-pill--failed cp-nodrag"
        onClick={() => navigate({ name: 'downloads' })}
        title={title}
        aria-label={`Open downloads. Install failed: ${title}`}
      >
        <StatusMark icon="alert" />
        <span class="cp-status-pill-label">Failed</span>
      </button>
    );
  }

  return (
    <span class="cp-status-pill cp-nodrag">
      <StatusMark icon="circle-dashed" />
      <span class="cp-status-pill-label">Idle</span>
    </span>
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
    <div class="cp-topbar cp-drag" onMouseDown={onDragAreaMouseDown} onDblClick={onDragAreaDoubleClick}>
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
            >
              {c.label}
            </button>
          </div>
        ))}
      </div>
      <div class="cp-topbar-spacer" />
      <div class="cp-topbar-actions cp-nodrag">
        <StatusPill />
        <MusicWidget />
      </div>
      <WindowControls />
    </div>
  );
}
