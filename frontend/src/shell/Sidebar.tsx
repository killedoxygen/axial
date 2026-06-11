import type { JSX } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { InstanceArt } from '../art/InstanceArt';
import { Icon } from '../ui/Icons';
import { PlayerHeadPreview } from '../ui/PlayerHeadPreview';
import { route, navigate, commandPaletteOpen, type Route, openCreate } from '../ui-state';
import { runningSessions, config, instances } from '../store';
import { promptPlayerName, savePlayerName } from '../player-name';
import { Music, musicStateVersion } from '../music';
import { local, saveLocalState } from '../state';
import { Sound } from '../sound';
import type { Instance } from '../types';

function isRouteActive(target: Route, current: Route): boolean {
  if (target.name !== current.name) return false;
  if ('id' in target || 'id' in current) return 'id' in target && 'id' in current && target.id === current.id;
  return true;
}

function recentTime(inst: Instance): number {
  const lastPlayed = inst.last_played_at ? Date.parse(inst.last_played_at) : 0;
  const created = Date.parse(inst.created_at);
  return Math.max(
    Number.isFinite(lastPlayed) ? lastPlayed : 0,
    Number.isFinite(created) ? created : 0,
  );
}

function RailButton({ icon, label, target, accent }: {
  icon: string;
  label: string;
  target: Route;
  accent?: boolean;
}): JSX.Element {
  const current = route.value;
  const active = isRouteActive(target, current)
    || (target.name === 'instances' && current.name === 'instance');
  return (
    <button
      class="cp-rail-btn"
      data-active={active}
      data-accent={accent}
      onClick={() => navigate(target)}
      title={label}
      aria-label={label}
    >
      <Icon name={icon} size={20} stroke={1.7} />
    </button>
  );
}

function RailInstances(): JSX.Element | null {
  const current = route.value;
  const list = [...instances.value]
    .sort((a, b) => recentTime(b) - recentTime(a) || a.name.localeCompare(b.name));
  if (list.length === 0) return null;
  return (
    <div class="cp-rail-instances">
      {list.map(inst => {
        const active = current.name === 'instance' && current.id === inst.id;
        const running = !!runningSessions.value[inst.id];
        return (
          <button
            key={inst.id}
            class="cp-rail-tile"
            data-active={active}
            data-running={running}
            onClick={() => navigate({ name: 'instance', id: inst.id })}
            title={inst.name}
            aria-label={inst.name}
          >
            <InstanceArt instance={inst} aspect="thumb" radius={12} className="cp-rail-tile-art" />
            {running && <span class="cp-rail-tile-dot" aria-hidden="true" />}
          </button>
        );
      })}
    </div>
  );
}

function UserMenu({ onClose }: { onClose: () => void }): JSX.Element {
  musicStateVersion.value;
  const musicOn = Music.enabled;
  const soundsOn = local.sounds;

  const renameUser = async (): Promise<void> => {
    const current = config.value?.username || 'Player';
    const next = await promptPlayerName(current);
    if (next) await savePlayerName(next);
    onClose();
  };

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
    icon, label, onSelect, hint, right,
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
      <MenuRow icon="edit" label="Change display name" onSelect={renameUser} />
      <MenuRow icon="user" label="Accounts and skins" onSelect={() => { navigate({ name: 'accounts' }); onClose(); }} />
      <div class="cp-userm-divider" />
      <MenuRow
        icon={soundsOn ? 'volume' : 'volume-off'}
        label="UI sounds"
        onSelect={toggleSounds}
        right={<span class="cp-userm-pill" data-on={soundsOn}>{soundsOn ? 'On' : 'Off'}</span>}
      />
      <MenuRow
        icon={musicOn ? 'music' : 'music-off'}
        label="Background music"
        onSelect={toggleMusic}
        right={<span class="cp-userm-pill" data-on={musicOn}>{musicOn ? 'On' : 'Off'}</span>}
      />
      <div class="cp-userm-divider" />
      <MenuRow icon="settings" label="Open settings" onSelect={() => { navigate({ name: 'settings' }); onClose(); }} hint="Ctrl ," />
    </div>
  );
}

function UserTrigger(): JSX.Element {
  const [open, setOpen] = useState(false);
  const username = (config.value?.username || 'Player').slice(0, 24);
  const rootRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onClick = (e: MouseEvent): void => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent): void => { if (e.key === 'Escape') setOpen(false); };
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
        onClick={() => setOpen(o => !o)}
        title={username}
        aria-label={`${username} — account menu`}
      >
        <PlayerHeadPreview username={username} size={34} radius={11} />
      </button>
    </div>
  );
}

export function Sidebar(): JSX.Element {
  return (
    <aside class="cp-rail">
      <div class="cp-rail-brand" title="Croopor">
        <img class="cp-logo" src="logo.png" alt="" width="26" height="26" />
      </div>
      <button
        class="cp-rail-btn"
        onClick={() => { commandPaletteOpen.value = true; }}
        title="Search (Ctrl K)"
        data-sound-silent="true"
        aria-label="Search and jump to"
      >
        <Icon name="search" size={20} stroke={1.7} />
      </button>
      <RailButton icon="home" label="Home" target={{ name: 'home' }} />
      <RailButton icon="cube" label="Instances" target={{ name: 'instances' }} />
      <button
        class="cp-rail-btn"
        data-accent="true"
        onClick={openCreate}
        title="New instance"
        aria-label="New instance"
      >
        <Icon name="plus" size={20} stroke={1.7} />
      </button>
      <div class="cp-rail-sep" aria-hidden="true" />
      <RailInstances />
      <div class="cp-rail-spacer" />
      <RailButton icon="settings" label="Settings" target={{ name: 'settings' }} />
      <UserTrigger />
    </aside>
  );
}
