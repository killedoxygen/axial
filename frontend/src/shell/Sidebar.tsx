import type { JSX } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { Icon } from '../ui/Icons';
import { Input } from '../ui/Atoms';
import { route, navigate, commandPaletteOpen, type Route } from '../ui-state';
import { installQueue, runningSessions, config, searchQuery } from '../store';
import { prompt } from '../ui/Dialog';
import { api } from '../api';
import { toast } from '../toast';
import { errMessage } from '../utils';
import { Music, musicStateVersion } from '../music';
import { local, saveLocalState } from '../state';
import { Sound } from '../sound';

interface SidebarItem {
  icon: string;
  label: string;
  route: Route;
  badge?: number;
}

interface SidebarGroup { title: string; items: SidebarItem[]; }

function SidebarItemBtn({ item }: { item: SidebarItem }): JSX.Element {
  const current = route.value;
  const active = current.name === item.route.name;
  return (
    <button
      class="cp-sidebar-item"
      data-active={active}
      onClick={() => navigate(item.route)}
    >
      <Icon name={item.icon} size={17} stroke={1.7} />
      <span class="cp-sidebar-label">{item.label}</span>
      {item.badge != null && item.badge > 0 && (
        <span class="cp-sidebar-badge">{item.badge}</span>
      )}
    </button>
  );
}

function UserMenu({ onClose }: { onClose: () => void }): JSX.Element {
  musicStateVersion.value;
  const musicOn = Music.enabled;
  const soundsOn = local.sounds;

  const renameUser = async (): Promise<void> => {
    const current = config.value?.username || 'Player';
    const next = await prompt('Display name', current, { title: 'Change name', placeholder: 'Your gamertag', confirmText: 'Save' });
    if (!next || !next.trim() || next === current) { onClose(); return; }
    try {
      const res: any = await api('PUT', '/config', { username: next.trim() });
      if (res.error) throw new Error(res.error);
      config.value = res;
      toast('Name updated');
    } catch (err) {
      toast(`Failed: ${errMessage(err)}`, 'error');
    }
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
        icon={soundsOn ? 'volume' : 'volume'}
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
  const initial = username[0]?.toUpperCase() || 'P';
  const running = Object.keys(runningSessions.value).length;
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
        class="cp-sidebar-user"
        type="button"
        data-open={open}
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen(o => !o)}
      >
        <div class="cp-avatar">{initial}</div>
        <div class="cp-sidebar-user-body">
          <div class="cp-sidebar-user-name">{username}</div>
          <div class="cp-sidebar-user-sub">{running > 0 ? `${running} playing` : 'online'}</div>
        </div>
        <Icon name="chevron-up" size={14} color="var(--text-mute)" style={{
          transform: open ? 'rotate(0deg)' : 'rotate(180deg)',
          transition: 'transform 160ms ease',
        }} />
      </button>
    </div>
  );
}

export function Sidebar(): JSX.Element {
  const queue = installQueue.value;

  const groups: SidebarGroup[] = [
    {
      title: 'Play',
      items: [
        { icon: 'home', label: 'Home', route: { name: 'home' } },
        { icon: 'cube', label: 'Instances', route: { name: 'instances' } },
        { icon: 'plus', label: 'New instance', route: { name: 'create' } },
      ],
    },
    {
      title: 'Discover',
      items: [
        { icon: 'compass', label: 'Browse', route: { name: 'browse' } },
        { icon: 'download', label: 'Downloads', route: { name: 'downloads' }, badge: queue.length },
      ],
    },
    {
      title: 'You',
      items: [
        { icon: 'user', label: 'Accounts & skins', route: { name: 'accounts' } },
        { icon: 'settings', label: 'Settings', route: { name: 'settings' } },
      ],
    },
  ];

  return (
    <aside class="cp-sidebar">
      <div class="cp-sidebar-brand">
        <img class="cp-logo" src="logo.svg" alt="" width="22" height="22" />
        <span class="cp-brand-name">Croopor</span>
      </div>
      <div class="cp-sidebar-search">
        <Input
          value={searchQuery.value}
          onChange={(v) => { searchQuery.value = v; }}
          placeholder="Search instances…"
          icon="search"
          onKeyDown={(e) => {
            if (e.key === 'Enter') { commandPaletteOpen.value = true; }
            if (e.key === 'Escape') { searchQuery.value = ''; }
          }}
        />
      </div>
      {groups.map(g => (
        <div class="cp-sidebar-group" key={g.title}>
          <div class="cp-sidebar-group-title">{g.title}</div>
          <div class="cp-sidebar-items">
            {g.items.map(it => <SidebarItemBtn key={it.label} item={it} />)}
          </div>
        </div>
      ))}
      <div class="cp-sidebar-spacer" />
      <UserTrigger />
    </aside>
  );
}
