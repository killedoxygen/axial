import type { JSX } from 'preact';
import { useCallback, useEffect, useRef, useState } from 'preact/hooks';
import { InstanceTile } from '../ui/InstanceVisual';
import { Button } from '../ui/Atoms';
import { Icon } from '../ui/Icons';
import { Logo } from '../ui/Logo';
import { PlayerHeadPreview } from '../ui/PlayerHeadPreview';
import { route, navigate, commandPaletteOpen, type Route, openCreate, openAccountSwitcher } from '../ui-state';
import { runningSessions, config, instances, updateInfo, versionById } from '../store';
import { instanceInstallStatus } from '../instance-install-status';
import { accountDisplayName, accountSkinSrc } from '../player-skin';
import { Music, musicStateVersion } from '../music';
import { local, saveLocalState } from '../state';
import { Sound } from '../sound';
import { openInstanceContextMenu } from '../views/instance/instance-menu';
import type { Instance } from '../types-instance';
import type { UpdateFlowState } from '../types-update';
import { shortcutHint } from '../shortcuts';
import {
  applyUpdateAndRestart,
  canInstallUpdateInApp,
  dismissAvailableUpdate,
  hasVisibleUpdate,
  openUpdateAction,
  openUpdateNotes,
  restartBlockedByActivity,
  restartDesktopApp,
  startUpdateDownload,
  updateFlow,
} from '../updater';
import { formatBytes } from '../utils';

type RailTip = {
  label: string;
  top: number;
};

type RailTooltipController = {
  show: (label: string, target: HTMLElement) => void;
  hide: () => void;
};
type RailTipEvent = JSX.TargetedEvent<HTMLElement>;

function isRouteActive(target: Route, current: Route): boolean {
  if (target.name !== current.name) return false;
  if ('id' in target || 'id' in current) return 'id' in target && 'id' in current && target.id === current.id;
  return true;
}

function recentTime(inst: Instance): number {
  const lastPlayed = inst.last_played_at ? Date.parse(inst.last_played_at) : 0;
  const created = Date.parse(inst.created_at);
  return Math.max(Number.isFinite(lastPlayed) ? lastPlayed : 0, Number.isFinite(created) ? created : 0);
}

function railTipAttrs(label: string, tooltip: RailTooltipController) {
  return {
    'data-rail-label': label,
    onMouseEnter: (e: RailTipEvent) => tooltip.show(label, e.currentTarget),
    onMouseLeave: tooltip.hide,
    onFocus: (e: RailTipEvent) => tooltip.show(label, e.currentTarget),
    onBlur: tooltip.hide,
  };
}

function RailIcon({ name, size = 20 }: { name: string; size?: number }): JSX.Element {
  if (name === 'stack') {
    return (
      <span class="cp-rail-icon" aria-hidden="true">
        <svg
          class="cp-rail-icon-svg cp-rail-stack"
          width={size}
          height={size}
          viewBox="0 0 24 24"
          fill="currentColor"
          focusable="false"
        >
          <path
            class="cp-rail-stack-layer cp-rail-stack-layer--top"
            d="M4.948 4.683A2 2 0 0 1 6.454 4h11.092a2 2 0 0 1 1.505.683l3.5 4C23.683 9.976 22.764 12 21.046 12H2.954C1.235 12 .317 9.976 1.448 8.683l3.5-4ZM17.546 6H6.454l-3.5 4h18.092l-3.5-4Z"
          />
          <path
            class="cp-rail-stack-layer cp-rail-stack-layer--mid"
            d="M2 15a1 1 0 0 1 1-1h18a1 1 0 1 1 0 2H3a1 1 0 0 1-1-1Z"
          />
          <path
            class="cp-rail-stack-layer cp-rail-stack-layer--base"
            d="M3 19a1 1 0 0 1 1-1h16a1 1 0 1 1 0 2H4a1 1 0 0 1-1-1Z"
          />
        </svg>
      </span>
    );
  }

  return (
    <span class="cp-rail-icon" aria-hidden="true">
      <Icon name={name} size={size} stroke={1.7} />
    </span>
  );
}

function RailButton({
  icon,
  label,
  target,
  accent,
  tooltip,
}: {
  icon: string;
  label: string;
  target: Route;
  accent?: boolean;
  tooltip: RailTooltipController;
}): JSX.Element {
  const current = route.value;
  const active = isRouteActive(target, current) || (target.name === 'instances' && current.name === 'instance');
  return (
    <button
      class="cp-rail-btn"
      data-active={active}
      data-accent={accent}
      data-icon={icon}
      onClick={() => {
        tooltip.hide();
        navigate(target);
      }}
      aria-label={label}
      {...railTipAttrs(label, tooltip)}
    >
      <RailIcon name={icon} />
    </button>
  );
}

function RailInstances({ tooltip }: { tooltip: RailTooltipController }): JSX.Element | null {
  const current = route.value;
  const listRef = useRef<HTMLDivElement>(null);
  const [scrollCue, setScrollCue] = useState({ top: false, bottom: false });
  const list = [...instances.value].sort((a, b) => recentTime(b) - recentTime(a) || a.name.localeCompare(b.name));
  const updateScrollCue = useCallback(() => {
    const node = listRef.current;
    if (!node) {
      setScrollCue({ top: false, bottom: false });
      return;
    }

    const scrollable = node.scrollHeight > node.clientHeight + 1;
    const next = {
      top: scrollable && node.scrollTop > 2,
      bottom: scrollable && node.scrollTop < node.scrollHeight - node.clientHeight - 2,
    };
    setScrollCue((currentCue) =>
      currentCue.top === next.top && currentCue.bottom === next.bottom ? currentCue : next,
    );
  }, []);

  useEffect(() => {
    updateScrollCue();
    const node = listRef.current;
    if (!node) return undefined;

    let resizeObserver: ResizeObserver | null = null;
    if (typeof ResizeObserver !== 'undefined') {
      resizeObserver = new ResizeObserver(updateScrollCue);
      resizeObserver.observe(node);
    }

    window.addEventListener('resize', updateScrollCue);
    return () => {
      resizeObserver?.disconnect();
      window.removeEventListener('resize', updateScrollCue);
    };
  }, [list.length, updateScrollCue]);

  if (list.length === 0) return null;
  return (
    <>
      <div class="cp-rail-sep" aria-hidden="true" />
      <div
        class="cp-rail-instances-shell"
        data-rail-instances-fade-top={scrollCue.top ? 'visible' : 'hidden'}
        data-rail-instances-fade-bottom={scrollCue.bottom ? 'visible' : 'hidden'}
      >
        <div class="cp-rail-instances" ref={listRef} onScroll={updateScrollCue}>
          {list.map((inst) => {
            const active = current.name === 'instance' && current.id === inst.id;
            const running = !!runningSessions.value[inst.id];
            const version = versionById(inst.version_id);
            const install = instanceInstallStatus(inst, version);
            const installing = install.installing;
            const installLabel = install.state === 'queued' ? install.queuedItem?.title || install.label : 'Installing';
            return (
              <button
                key={inst.id}
                class="cp-rail-tile"
                data-active={active}
                data-running={running}
                data-installing={installing}
                onClick={() => {
                  tooltip.hide();
                  navigate({ name: 'instance', id: inst.id });
                }}
                onContextMenu={(e) => {
                  tooltip.hide();
                  openInstanceContextMenu(e, inst);
                }}
                aria-label={installing ? `${inst.name}: ${installLabel}` : inst.name}
                {...railTipAttrs(installing ? `${inst.name} · ${installLabel}` : inst.name, tooltip)}
              >
                <InstanceTile inst={inst} radius={12} className="cp-rail-tile-art" />
                {installing && (
                  <span class="cp-rail-tile-install" aria-hidden="true">
                    <Icon name={install.state === 'queued' ? 'clock' : 'download'} size={10} stroke={2.4} />
                  </span>
                )}
                {running && <span class="cp-rail-tile-dot" aria-hidden="true" />}
              </button>
            );
          })}
        </div>
        <span class="cp-rail-instances-fade cp-rail-instances-fade--top" aria-hidden="true" />
        <span class="cp-rail-instances-fade cp-rail-instances-fade--bottom" aria-hidden="true" />
      </div>
    </>
  );
}

const UPDATE_RING_CIRCUMFERENCE = 2 * Math.PI * 19.5;

function displayUpdateVersion(version: string): string {
  if (!version) return '';
  return version.startsWith('v') || version.startsWith('V') ? version : `v${version}`;
}

function railUpdateTipLabel(flowState: UpdateFlowState, latest: string): string {
  switch (flowState.phase) {
    case 'downloading':
      return flowState.percent != null ? `Downloading update · ${flowState.percent}%` : 'Downloading update';
    case 'applying':
      return 'Installing update';
    case 'ready':
      return `Restart to update to ${latest}`;
    case 'restart-pending':
      return 'Restart Axial to finish the update';
    default:
      return `Update ${latest} available`;
  }
}

function UpdatePopover({ latest, onClose }: { latest: string; onClose: () => void }): JSX.Element {
  const info = updateInfo.value;
  const flowState = updateFlow.value;
  const busy = flowState.phase === 'downloading' || flowState.phase === 'applying';
  const current = displayUpdateVersion(info?.current_version || '');
  const restartBlocked = restartBlockedByActivity();
  const inApp = canInstallUpdateInApp();

  const title =
    flowState.phase === 'applying'
      ? 'Installing update'
      : flowState.phase === 'downloading'
        ? 'Downloading update'
        : flowState.phase === 'ready'
          ? 'Update downloaded'
          : flowState.phase === 'restart-pending'
            ? 'Restart to finish'
            : flowState.phase === 'failed'
              ? 'Update failed'
              : 'Update available';

  const download = (): void => {
    void startUpdateDownload();
    if (!inApp) onClose();
  };

  return (
    <div class="cp-update-pop" role="dialog" aria-label="App update">
      <div class="cp-update-pop-head">
        <span class="cp-update-pop-title">{title}</span>
        {current && latest && (
          <span class="cp-update-pop-versions">
            {current} <Icon name="arrow-right" size={11} stroke={2} /> {latest}
          </span>
        )}
      </div>
      {busy && (
        <div class="cp-update-pop-progress">
          <div
            class="cp-update-pop-bar"
            data-indeterminate={flowState.percent == null || flowState.phase === 'applying'}
            role="progressbar"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={flowState.percent ?? undefined}
          >
            <span
              class="cp-update-pop-bar-fill"
              style={flowState.percent != null ? { width: `${flowState.percent}%` } : undefined}
            />
          </div>
          <span class="cp-update-pop-progress-label">
            {flowState.phase === 'applying'
              ? 'Installing...'
              : flowState.total_bytes
                ? `${formatBytes(flowState.received_bytes)} of ${formatBytes(flowState.total_bytes)}`
                : formatBytes(flowState.received_bytes)}
          </span>
        </div>
      )}
      {flowState.phase === 'failed' && flowState.message && (
        <div class="cp-update-pop-note" data-tone="error">
          {flowState.message}
        </div>
      )}
      {flowState.phase === 'ready' && (
        <div class="cp-update-pop-note">
          {restartBlocked ? 'Waiting for downloads and running games to finish.' : `Axial will restart into ${latest}.`}
        </div>
      )}
      {flowState.phase === 'restart-pending' && (
        <div class="cp-update-pop-note">The update is applied and takes effect on the next start.</div>
      )}
      <div class="cp-update-pop-actions">
        {(flowState.phase === 'idle' || flowState.phase === 'failed') && (
          <>
            <Button variant="primary" size="sm" icon="download" onClick={download}>
              {flowState.phase === 'failed'
                ? 'Try again'
                : inApp
                  ? 'Download update'
                  : info?.action_label || 'Open release'}
            </Button>
            <Button variant="ghost" size="sm" icon="tag" onClick={() => void openUpdateNotes()}>
              Notes
            </Button>
            <Button
              variant="ghost"
              size="sm"
              onClick={() => {
                dismissAvailableUpdate();
                onClose();
              }}
            >
              Skip
            </Button>
          </>
        )}
        {flowState.phase === 'ready' && (
          <>
            <Button
              variant="primary"
              size="sm"
              icon="refresh"
              disabled={restartBlocked}
              onClick={() => void applyUpdateAndRestart()}
            >
              Restart to update
            </Button>
            <Button variant="ghost" size="sm" icon="tag" onClick={() => void openUpdateNotes()}>
              Notes
            </Button>
          </>
        )}
        {flowState.phase === 'restart-pending' && (
          <Button variant="primary" size="sm" icon="refresh" onClick={() => void restartDesktopApp()}>
            Restart now
          </Button>
        )}
      </div>
    </div>
  );
}

function RailUpdate({ tooltip }: { tooltip: RailTooltipController }): JSX.Element | null {
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  const flowState = updateFlow.value;

  useEffect(() => {
    if (!open) return;
    const onClick = (e: MouseEvent): void => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === 'Escape') setOpen(false);
    };
    document.addEventListener('mousedown', onClick);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onClick);
      document.removeEventListener('keydown', onKey);
    };
  }, [open]);

  const busy = flowState.phase === 'downloading' || flowState.phase === 'applying';
  const staged = flowState.phase === 'ready' || flowState.phase === 'restart-pending';
  if (!busy && !staged && !hasVisibleUpdate()) return null;

  const latest = displayUpdateVersion(flowState.version || updateInfo.value?.latest_version || '');
  const tipLabel = railUpdateTipLabel(flowState, latest);
  const icon = busy ? 'download' : staged ? 'check-circle' : 'sparkles';
  const pct = flowState.percent;
  const ringOffset =
    pct != null
      ? UPDATE_RING_CIRCUMFERENCE * (1 - Math.min(100, Math.max(0, pct)) / 100)
      : UPDATE_RING_CIRCUMFERENCE * 0.72;

  return (
    <div class="cp-update-shell" ref={rootRef}>
      {open && <UpdatePopover latest={latest} onClose={() => setOpen(false)} />}
      <button
        class="cp-rail-btn cp-rail-update"
        data-update-state={busy || staged ? flowState.phase : 'available'}
        data-open={open}
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-label={tipLabel}
        onClick={() => {
          tooltip.hide();
          setOpen((o) => !o);
        }}
        {...railTipAttrs(tipLabel, tooltip)}
      >
        <span class="cp-rail-update-glyph" key={icon}>
          <RailIcon name={icon} />
        </span>
        {busy && (
          <svg class="cp-rail-update-ring" viewBox="0 0 44 44" data-indeterminate={pct == null} aria-hidden="true">
            <circle class="cp-rail-update-ring-track" cx="22" cy="22" r="19.5" />
            <circle
              class="cp-rail-update-ring-bar"
              cx="22"
              cy="22"
              r="19.5"
              style={{ strokeDasharray: UPDATE_RING_CIRCUMFERENCE, strokeDashoffset: ringOffset }}
            />
          </svg>
        )}
        {!busy && !staged && <span class="cp-rail-update-dot" aria-hidden="true" />}
      </button>
    </div>
  );
}

function UserMenu({ onClose }: { onClose: () => void }): JSX.Element {
  musicStateVersion.value;
  const musicOn = Music.enabled;
  const soundsOn = local.sounds;

  const toggleSounds = (): void => {
    const next = !soundsOn;
    local.sounds = next;
    Sound.enabled = next;
    saveLocalState();
    if (next) Sound.ui('affirm');
  };

  const toggleMusic = (): void => {
    Music.toggle();
  };

  const MenuRow = ({
    icon,
    label,
    onSelect,
    hint,
    right,
  }: {
    icon: string;
    label: string;
    onSelect: () => void;
    hint?: string;
    right?: JSX.Element;
  }): JSX.Element => (
    <button class="cp-userm-row" onClick={onSelect}>
      <Icon name={icon} size={15} stroke={1.8} />
      <span class="cp-userm-label">{label}</span>
      {hint && <span class="cp-userm-hint">{hint}</span>}
      {right}
    </button>
  );

  return (
    <div class="cp-userm" role="menu">
      <MenuRow
        icon="refresh"
        label="Switch account"
        onSelect={() => {
          openAccountSwitcher();
          onClose();
        }}
      />
      <MenuRow
        icon="user"
        label="Accounts and skins"
        onSelect={() => {
          navigate({ name: 'accounts' });
          onClose();
        }}
      />
      <div class="cp-userm-divider" />
      <MenuRow
        icon={soundsOn ? 'volume' : 'volume-off'}
        label="UI sounds"
        onSelect={toggleSounds}
        right={
          <span class="cp-userm-pill" data-on={soundsOn}>
            {soundsOn ? 'On' : 'Off'}
          </span>
        }
      />
      <MenuRow
        icon={musicOn ? 'music' : 'music-off'}
        label="Background music"
        onSelect={toggleMusic}
        right={
          <span class="cp-userm-pill" data-on={musicOn}>
            {musicOn ? 'On' : 'Off'}
          </span>
        }
      />
      <div class="cp-userm-divider" />
      <MenuRow
        icon="settings"
        label="Open settings"
        onSelect={() => {
          navigate({ name: 'settings' });
          onClose();
        }}
        hint={shortcutHint('open-settings')}
      />
    </div>
  );
}

function UserTrigger({ tooltip }: { tooltip: RailTooltipController }): JSX.Element {
  const [open, setOpen] = useState(false);
  const username = (accountDisplayName.value || config.value?.username || 'Player').slice(0, 24);
  const rootRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onClick = (e: MouseEvent): void => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === 'Escape') setOpen(false);
    };
    document.addEventListener('mousedown', onClick);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onClick);
      document.removeEventListener('keydown', onKey);
    };
  }, [open]);

  return (
    <div class="cp-user-shell" ref={rootRef}>
      {open && <UserMenu onClose={() => setOpen(false)} />}
      <button
        class="cp-rail-user"
        type="button"
        data-open={open}
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => {
          tooltip.hide();
          setOpen((o) => !o);
        }}
        onPointerEnter={() => {
          void import('../views/accounts/AccountSwitcherHost');
        }}
        aria-label={`${username}, account menu`}
        {...railTipAttrs(username, tooltip)}
      >
        <PlayerHeadPreview username={username} textureSrc={accountSkinSrc.value ?? undefined} size={34} radius={11} />
      </button>
    </div>
  );
}

export function Sidebar(): JSX.Element {
  const [tip, setTip] = useState<RailTip | null>(null);
  const railRef = useRef<HTMLElement>(null);
  const tooltip: RailTooltipController = {
    show: (label, target) => {
      const railRect = railRef.current?.getBoundingClientRect();
      const targetRect = target.getBoundingClientRect();
      const top = railRect ? targetRect.top - railRect.top + targetRect.height / 2 : targetRect.height / 2;
      setTip({ label, top });
    },
    hide: () => setTip(null),
  };

  return (
    <aside class="cp-rail" ref={railRef}>
      <div class="cp-rail-brand" aria-hidden="true">
        <Logo className="cp-logo" size={34} />
      </div>
      <button
        class="cp-rail-btn"
        data-icon="search"
        onClick={() => {
          tooltip.hide();
          commandPaletteOpen.value = true;
        }}
        data-sound-silent="true"
        aria-label="Search and jump to"
        {...railTipAttrs('Search', tooltip)}
      >
        <RailIcon name="search" />
      </button>
      <RailButton icon="home" label="Home" target={{ name: 'home' }} tooltip={tooltip} />
      <RailButton icon="stack" label="Instances" target={{ name: 'instances' }} tooltip={tooltip} />
      <button
        class="cp-rail-btn"
        data-accent="true"
        data-icon="plus"
        onClick={() => {
          tooltip.hide();
          openCreate();
        }}
        aria-label="New instance"
        {...railTipAttrs('New instance', tooltip)}
      >
        <RailIcon name="plus" />
      </button>
      <RailInstances tooltip={tooltip} />
      <div class="cp-rail-spacer" />
      <RailUpdate tooltip={tooltip} />
      <RailButton icon="settings" label="Settings" target={{ name: 'settings' }} tooltip={tooltip} />
      <UserTrigger tooltip={tooltip} />
      {tip && (
        <div
          class="cp-rail-tip"
          style={{ '--cp-rail-tip-top': `${tip.top}px` } as JSX.CSSProperties}
          aria-hidden="true"
        >
          <span>{tip.label}</span>
        </div>
      )}
    </aside>
  );
}
