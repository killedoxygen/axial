import type { JSX } from 'preact';
import { Icon } from '../ui/Icons';
import { Input } from '../ui/Atoms';
import { route, navigate, commandPaletteOpen, type Route } from '../ui-state';
import { installQueue, runningSessions, config, searchQuery } from '../store';

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

export function Sidebar(): JSX.Element {
  const queue = installQueue.value;
  const running = Object.keys(runningSessions.value).length;
  const username = (config.value?.username || 'Player').slice(0, 24);
  const initial = username[0]?.toUpperCase() || 'P';

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
      <button
        class="cp-sidebar-user"
        type="button"
        onClick={() => navigate({ name: 'accounts' })}
        style={{ border: 'none', cursor: 'pointer', textAlign: 'left' }}
      >
        <div class="cp-avatar">{initial}</div>
        <div class="cp-sidebar-user-body">
          <div class="cp-sidebar-user-name">{username}</div>
          <div class="cp-sidebar-user-sub">{running > 0 ? `${running} playing` : 'online'}</div>
        </div>
        <Icon name="chevron-down" size={14} color="var(--text-mute)" />
      </button>
    </aside>
  );
}
