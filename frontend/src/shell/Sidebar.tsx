import type { JSX } from 'preact';
import { useRef, useState } from 'preact/hooks';
import { InstanceTile } from '../ui/InstanceVisual';
import { Kbd, Ring } from '../ui/Atoms';
import { Icon, type IconName } from '../ui/Icons';
import { Logo } from '../ui/Logo';
import { PlayerHeadPreview } from '../ui/PlayerHeadPreview';
import { useDismiss } from '../hooks/use-dismiss';
import { route, navigate, type Route, openCreate, openAccountSwitcher } from '../ui-state';
import { launchSessions, config, instances, versionById } from '../store';
import { instanceInstallStatus } from '../instance-install-status';
import { activeAccount } from '../machines/accounts-state';
import { accountDisplayName, accountSkinSrc } from '../player-skin';
import { AccountHead, AccountKindChip, accountName } from '../views/accounts/account-face';
import { Music, musicStateVersion } from '../music';
import { local, saveLocalState, canEditPreferences } from '../state';
import { Sound } from '../sound';
import { openInstanceContextMenu } from '../views/instance/instance-menu';
import type { Instance } from '../types-instance';
import { shortcutHint } from '../shortcuts';
import { launchSessionActivityLabel, launchSessionIsPlaying } from '../launch-presenters';

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

function RailIcon({ name, size = 20 }: { name: IconName; size?: number }): JSX.Element {
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
  icon: IconName;
  label: string;
  target: Route;
  accent?: boolean;
  tooltip: RailTooltipController;
}): JSX.Element {
  const current = route.value;
  const active =
    isRouteActive(target, current) ||
    (target.name === 'instances' && current.name === 'instance') ||
    (target.name === 'discover' && current.name === 'content');
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

function RailTray({ tooltip }: { tooltip: RailTooltipController }): JSX.Element {
  const current = route.value;
  const list = [...instances.value].sort((a, b) => recentTime(b) - recentTime(a) || a.name.localeCompare(b.name));
  return (
    <div class="cp-rail-tray">
      {list.length > 0 && (
        <div class="cp-rail-instances">
          {list.map((inst) => {
            const active = current.name === 'instance' && current.id === inst.id;
            const session = launchSessions.value[inst.id];
            const playing = launchSessionIsPlaying(session);
            const sessionLabel = launchSessionActivityLabel(session);
            const version = versionById(inst.version_id);
            const install = instanceInstallStatus(inst, version);
            const installing = install.installing;
            const installLabel = install.state === 'queued' ? install.queuedItem?.title || install.label : 'Installing';
            return (
              <button
                key={inst.id}
                class="cp-rail-tile"
                data-active={active}
                data-running={playing}
                data-session-active={Boolean(session)}
                data-installing={installing}
                onClick={() => {
                  tooltip.hide();
                  navigate({ name: 'instance', id: inst.id });
                }}
                onContextMenu={(e) => {
                  tooltip.hide();
                  openInstanceContextMenu(e, inst);
                }}
                aria-label={
                  installing ? `${inst.name}: ${installLabel}` : session ? `${inst.name}: ${sessionLabel}` : inst.name
                }
                {...railTipAttrs(
                  installing ? `${inst.name}: ${installLabel}` : session ? `${inst.name}: ${sessionLabel}` : inst.name,
                  tooltip,
                )}
              >
                <InstanceTile inst={inst} className="cp-rail-tile-art" />
                {installing && <Ring size={44} progress={install.progress?.pct} />}
                {session && !installing && <span class="cp-rail-tile-dot" aria-hidden="true" />}
              </button>
            );
          })}
        </div>
      )}
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
    </div>
  );
}

function UserMenu({ onClose }: { onClose: () => void }): JSX.Element {
  musicStateVersion.value;
  const musicOn = Music.enabled;
  const soundsOn = local.sounds;

  const account = activeAccount();

  const toggleSounds = (): void => {
    if (!canEditPreferences()) return;
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
    right,
    checked,
  }: {
    icon: IconName;
    label: string;
    onSelect: () => void;
    right?: JSX.Element;
    /** A setting the row switches: shown as a switch, announced as a checked menu item. */
    checked?: boolean;
  }): JSX.Element => (
    <button
      class="cp-menu-item"
      role={checked === undefined ? 'menuitem' : 'menuitemcheckbox'}
      aria-checked={checked}
      onClick={onSelect}
    >
      <Icon name={icon} size={14} stroke={1.8} />
      <span>{label}</span>
      {right}
      {checked !== undefined && <span class="cp-toggle" data-on={checked} aria-hidden="true" />}
    </button>
  );

  return (
    <div class="cp-userm cp-pop cp-menu" role="menu">
      {account && (
        <div class="cp-userm-me">
          <AccountHead account={account} />
          <strong>{accountName(account)}</strong>
          <AccountKindChip account={account} />
        </div>
      )}
      <MenuRow
        icon="refresh"
        label="Switch account"
        right={<Icon name="chevron-right" size={14} />}
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
      <div class="cp-menu-gap" />
      <MenuRow icon={soundsOn ? 'volume' : 'volume-off'} label="UI sounds" onSelect={toggleSounds} checked={soundsOn} />
      <MenuRow
        icon={musicOn ? 'music' : 'music-off'}
        label="Background music"
        onSelect={toggleMusic}
        checked={musicOn}
      />
      <div class="cp-menu-gap" />
      <MenuRow
        icon="settings"
        label="Settings"
        onSelect={() => {
          navigate({ name: 'settings' });
          onClose();
        }}
        right={<Kbd>{shortcutHint('open-settings')}</Kbd>}
      />
    </div>
  );
}

function UserTrigger({ tooltip }: { tooltip: RailTooltipController }): JSX.Element {
  const [open, setOpen] = useState(false);
  const username = (accountDisplayName.value || config.value?.username || 'Player').slice(0, 24);
  const rootRef = useRef<HTMLDivElement>(null);

  useDismiss(rootRef, open, () => setOpen(false));

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
          void import('../views/accounts/AccountSwitcher');
        }}
        aria-label={`${username}, account menu`}
        {...railTipAttrs(username, tooltip)}
      >
        <PlayerHeadPreview textureSrc={accountSkinSrc.value ?? undefined} size={34} radius={10} />
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
      <RailButton icon="home" label="Home" target={{ name: 'home' }} tooltip={tooltip} />
      <RailButton icon="stack" label="Instances" target={{ name: 'instances' }} tooltip={tooltip} />
      <RailButton icon="compass" label="Discover" target={{ name: 'discover' }} tooltip={tooltip} />
      <RailTray tooltip={tooltip} />
      <div class="cp-rail-spacer" />
      <RailButton icon="settings" label="Settings" target={{ name: 'settings' }} tooltip={tooltip} />
      <UserTrigger tooltip={tooltip} />
      {tip && (
        <div
          class="cp-rail-tip cp-tip"
          style={{ '--cp-rail-tip-top': `${tip.top}px` } as JSX.CSSProperties}
          aria-hidden="true"
        >
          {tip.label}
        </div>
      )}
    </aside>
  );
}
