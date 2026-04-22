import type { JSX } from 'preact';
import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { Icon } from './Icons';
import { Kbd } from './Atoms';
import { commandPaletteOpen, navigate, type Route } from '../ui-state';
import { instances, runningSessions, config } from '../store';
import { Music } from '../music';
import { local, saveLocalState } from '../state';
import { Sound } from '../sound';
import { applyTheme } from '../theme';
import { prompt } from './Dialog';
import { api } from '../api';
import { toast } from '../toast';
import { errMessage } from '../utils';
import type { EnrichedInstance } from '../types';
import './command-palette.css';

type Group = 'jump' | 'instance' | 'action';

interface Command {
  id: string;
  group: Group;
  icon: string;
  label: string;
  hint?: string;
  keywords?: string;
  perform: () => void | Promise<void>;
}

const GROUP_LABELS: Record<Group, string> = {
  jump: 'Jump to',
  instance: 'Instances',
  action: 'Actions',
};

function buildCommands(): Command[] {
  const list: Command[] = [];
  const close = (): void => { commandPaletteOpen.value = false; };
  const goto = (r: Route, hint?: string): Command['perform'] => () => { navigate(r); close(); };

  list.push(
    { id: 'jump:home', group: 'jump', icon: 'home', label: 'Home', perform: goto({ name: 'home' }) },
    { id: 'jump:instances', group: 'jump', icon: 'cube', label: 'Instances', perform: goto({ name: 'instances' }) },
    { id: 'jump:create', group: 'jump', icon: 'plus', label: 'New instance', hint: 'Ctrl N', perform: goto({ name: 'create' }) },
    { id: 'jump:browse', group: 'jump', icon: 'compass', label: 'Browse', perform: goto({ name: 'browse' }) },
    { id: 'jump:downloads', group: 'jump', icon: 'download', label: 'Downloads', perform: goto({ name: 'downloads' }) },
    { id: 'jump:accounts', group: 'jump', icon: 'user', label: 'Accounts and skins', perform: goto({ name: 'accounts' }) },
    { id: 'jump:settings', group: 'jump', icon: 'settings', label: 'Settings', hint: 'Ctrl ,', perform: goto({ name: 'settings' }) },
  );

  const running = runningSessions.value;
  const list2 = instances.value as EnrichedInstance[];
  for (const inst of list2.slice(0, 12)) {
    const isRunning = !!running[inst.id];
    list.push({
      id: `instance:${inst.id}`,
      group: 'instance',
      icon: isRunning ? 'play' : 'cube',
      label: isRunning ? `Jump to ${inst.name}` : `Open ${inst.name}`,
      hint: isRunning ? 'Playing' : undefined,
      keywords: inst.name,
      perform: () => { navigate({ name: 'instance', id: inst.id }); close(); },
    });
  }

  const dark = local.lightness < 50;
  list.push(
    {
      id: 'action:mode',
      group: 'action',
      icon: 'palette',
      label: dark ? 'Switch to light mode' : 'Switch to dark mode',
      perform: () => {
        applyTheme(local.theme || 'custom', null, { lightness: dark ? 60 : 0 });
        close();
      },
    },
    {
      id: 'action:music',
      group: 'action',
      icon: Music.enabled ? 'music-off' : 'music',
      label: Music.enabled ? 'Mute background music' : 'Play background music',
      perform: () => { Music.toggle(); close(); },
    },
    {
      id: 'action:sounds',
      group: 'action',
      icon: 'headphones',
      label: local.sounds ? 'Turn UI sounds off' : 'Turn UI sounds on',
      perform: () => {
        local.sounds = !local.sounds;
        Sound.enabled = local.sounds;
        saveLocalState();
        if (local.sounds) Sound.ui('affirm');
        close();
      },
    },
    {
      id: 'action:name',
      group: 'action',
      icon: 'edit',
      label: 'Change display name',
      perform: async () => {
        close();
        const current = config.value?.username || 'Player';
        const next = await prompt('Display name', current, { title: 'Change name', placeholder: 'Your gamertag', confirmText: 'Save' });
        if (!next || !next.trim() || next === current) return;
        try {
          const res: any = await api('PUT', '/config', { username: next.trim() });
          if (res.error) throw new Error(res.error);
          config.value = res;
          toast('Name updated');
        } catch (err) {
          toast(`Failed: ${errMessage(err)}`, 'error');
        }
      },
    },
    {
      id: 'action:reload',
      group: 'action',
      icon: 'refresh',
      label: 'Reload launcher',
      hint: 'F5',
      perform: () => { location.reload(); },
    },
  );

  return list;
}

function score(cmd: Command, q: string): number {
  if (!q) return 1;
  const hay = `${cmd.label} ${cmd.keywords || ''}`.toLowerCase();
  const nq = q.toLowerCase();
  if (hay.startsWith(nq)) return 4;
  if (hay.includes(` ${nq}`)) return 3;
  if (hay.includes(nq)) return 2;
  let i = 0;
  for (const ch of nq) {
    i = hay.indexOf(ch, i);
    if (i === -1) return 0;
    i += 1;
  }
  return 1;
}

export function CommandPalette(): JSX.Element | null {
  const open = commandPaletteOpen.value;
  const [query, setQuery] = useState('');
  const [active, setActive] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);

  const commands = useMemo(() => (open ? buildCommands() : []), [open]);

  const filtered = useMemo(() => {
    const scored = commands
      .map(c => ({ cmd: c, s: score(c, query) }))
      .filter(x => x.s > 0);
    scored.sort((a, b) => {
      if (b.s !== a.s) return b.s - a.s;
      const ga = a.cmd.group === 'instance' ? 0 : a.cmd.group === 'jump' ? 1 : 2;
      const gb = b.cmd.group === 'instance' ? 0 : b.cmd.group === 'jump' ? 1 : 2;
      return ga - gb;
    });
    return scored.map(x => x.cmd);
  }, [commands, query]);

  useEffect(() => { if (open) { setQuery(''); setActive(0); } }, [open]);
  useEffect(() => { setActive(a => Math.min(a, Math.max(0, filtered.length - 1))); }, [filtered.length]);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === 'Escape') {
        e.preventDefault();
        commandPaletteOpen.value = false;
        return;
      }
      if (e.key === 'ArrowDown') {
        e.preventDefault();
        setActive(a => Math.min(filtered.length - 1, a + 1));
        return;
      }
      if (e.key === 'ArrowUp') {
        e.preventDefault();
        setActive(a => Math.max(0, a - 1));
        return;
      }
      if (e.key === 'Enter') {
        e.preventDefault();
        const cmd = filtered[active];
        if (cmd) void cmd.perform();
        return;
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [open, active, filtered]);

  useEffect(() => {
    const el = listRef.current?.querySelector<HTMLElement>(`[data-idx="${active}"]`);
    if (el) el.scrollIntoView({ block: 'nearest' });
  }, [active]);

  if (!open) return null;

  const grouped: Array<{ group: Group; items: Array<{ cmd: Command; idx: number }> }> = [];
  const bucket = new Map<Group, Array<{ cmd: Command; idx: number }>>();
  filtered.forEach((cmd, idx) => {
    const arr = bucket.get(cmd.group) || [];
    arr.push({ cmd, idx });
    bucket.set(cmd.group, arr);
  });
  (['instance', 'jump', 'action'] as Group[]).forEach(g => {
    const items = bucket.get(g);
    if (items && items.length > 0) grouped.push({ group: g, items });
  });

  return (
    <div
      class="cp-cmd-overlay"
      onClick={(e) => { if (e.target === e.currentTarget) commandPaletteOpen.value = false; }}
    >
      <div class="cp-cmd" role="dialog" aria-modal="true" aria-label="Command palette">
        <div class="cp-cmd-head">
          <Icon name="search" size={15} color="var(--text-dim)" />
          <input
            class="cp-cmd-input"
            autoFocus
            placeholder="Jump to…"
            value={query}
            onInput={(e: any) => setQuery(e.currentTarget.value)}
          />
          <Kbd>esc</Kbd>
        </div>
        <div class="cp-cmd-list" ref={listRef}>
          {filtered.length === 0 ? (
            <div class="cp-cmd-empty">
              <Icon name="search" size={20} color="var(--text-mute)" />
              <span>No matches</span>
            </div>
          ) : grouped.map(section => (
            <div key={section.group} class="cp-cmd-section">
              <div class="cp-cmd-section-title">{GROUP_LABELS[section.group]}</div>
              {section.items.map(({ cmd, idx }) => (
                <button
                  key={cmd.id}
                  class="cp-cmd-item"
                  data-idx={idx}
                  data-active={idx === active}
                  onMouseMove={() => setActive(idx)}
                  onClick={() => { void cmd.perform(); }}
                  data-sound-silent="true"
                >
                  <Icon name={cmd.icon} size={15} stroke={1.8} />
                  <span class="cp-cmd-label">{cmd.label}</span>
                  {cmd.hint && <span class="cp-cmd-hint">{cmd.hint}</span>}
                </button>
              ))}
            </div>
          ))}
        </div>
        <div class="cp-cmd-foot">
          <span class="cp-cmd-foot-hint"><Kbd>↑</Kbd><Kbd>↓</Kbd> move</span>
          <span class="cp-cmd-foot-hint"><Kbd>↵</Kbd> select</span>
          <span class="cp-cmd-foot-hint"><Kbd>esc</Kbd> close</span>
        </div>
      </div>
    </div>
  );
}
