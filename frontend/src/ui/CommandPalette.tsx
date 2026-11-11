import type { JSX } from 'preact';
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'preact/hooks';
import { useDraggableOverlay } from '../hooks/use-draggable-overlay';
import { Music } from '../music';
import { reloadApplication } from '../preferences/persistence';
import { comboParts, effectiveCombos, shortcutById, type ShortcutId } from '../shortcuts';
import { Sound } from '../sound';
import { canEditPreferences, local, saveLocalState } from '../state';
import { instances, launchSessions } from '../store';
import { applyTheme } from '../theme';
import type { EnrichedInstance } from '../types-instance';
import type { LaunchSession } from '../types-launch';
import { commandPaletteOpen, navigate, openAccountSwitcher, openCreate, type Route } from '../ui-state';
import { Kbd, KeyLegend, Keys } from './Atoms';
import { Face } from './Face';
import { Icon, type IconName } from './Icons';
import { InstanceFacts, SessionChip } from './InstanceFacts';
import { InstanceTile } from './InstanceVisual';

type Group = 'instance' | 'jump' | 'action';

const GROUPS: ReadonlyArray<{ id: Group; label: string; verb: string }> = [
  { id: 'instance', label: 'Instances', verb: 'Open' },
  { id: 'jump', label: 'Jump to', verb: 'Open' },
  { id: 'action', label: 'Actions', verb: 'Run' },
];

const INSTANCE_LIMIT = 12;
const LEGEND = [
  [['↑', '↓'], 'move'],
  [['↵'], 'select'],
] as const;

interface Command {
  id: string;
  group: Group;
  label: string;
  /** Matched with the label, never shown. */
  keywords?: string;
  icon?: IconName;
  instance?: EnrichedInstance;
  session?: LaunchSession;
  keys?: string[];
  perform: () => void | Promise<void>;
}

function shortcutKeys(id: ShortcutId): string[] {
  return comboParts(effectiveCombos(shortcutById(id))[0]!);
}

function close(): void {
  commandPaletteOpen.value = false;
}

function buildCommands(): Command[] {
  const go = (route: Route) => (): void => {
    navigate(route);
    close();
  };
  const sessions = launchSessions.value;
  const dark = local.lightness < 50;

  return [
    ...instances.value.slice(0, INSTANCE_LIMIT).map((instance): Command => {
      const session = sessions[instance.id];
      const { loader_label, minecraft_label } = instance.version_display;
      return {
        id: `instance:${instance.id}`,
        group: 'instance',
        label: instance.name,
        keywords: `${loader_label} ${minecraft_label}`,
        instance,
        session,
        perform: go({ name: 'instance', id: instance.id }),
      };
    }),
    { id: 'jump:home', group: 'jump', icon: 'home', label: 'Home', perform: go({ name: 'home' }) },
    { id: 'jump:instances', group: 'jump', icon: 'stack', label: 'Instances', perform: go({ name: 'instances' }) },
    {
      id: 'jump:discover',
      group: 'jump',
      icon: 'compass',
      label: 'Discover mods and packs',
      perform: go({ name: 'discover' }),
    },
    {
      id: 'jump:create',
      group: 'jump',
      icon: 'plus',
      label: 'New instance',
      keys: shortcutKeys('new-instance'),
      perform: () => {
        openCreate();
        close();
      },
    },
    { id: 'jump:downloads', group: 'jump', icon: 'download', label: 'Downloads', perform: go({ name: 'downloads' }) },
    {
      id: 'jump:accounts',
      group: 'jump',
      icon: 'user',
      label: 'Accounts and skins',
      perform: go({ name: 'accounts' }),
    },
    {
      id: 'jump:settings',
      group: 'jump',
      icon: 'settings',
      label: 'Settings',
      keys: shortcutKeys('open-settings'),
      perform: go({ name: 'settings' }),
    },
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
      perform: () => {
        Music.toggle();
        close();
      },
    },
    {
      id: 'action:sounds',
      group: 'action',
      icon: 'headphones',
      label: local.sounds ? 'Turn UI sounds off' : 'Turn UI sounds on',
      perform: () => {
        if (!canEditPreferences()) return;
        local.sounds = !local.sounds;
        Sound.enabled = local.sounds;
        saveLocalState();
        if (local.sounds) Sound.ui('affirm');
        close();
      },
    },
    {
      id: 'action:account',
      group: 'action',
      icon: 'refresh',
      label: 'Switch account',
      perform: () => {
        openAccountSwitcher();
        close();
      },
    },
    {
      id: 'action:reload',
      group: 'action',
      icon: 'refresh',
      label: 'Reload launcher',
      keys: ['F5'],
      perform: () => void reloadApplication(),
    },
  ];
}

function score(command: Command, query: string): number {
  if (!query) return 1;
  const hay = `${command.label} ${command.keywords ?? ''}`.toLowerCase();
  if (hay.startsWith(query)) return 4;
  if (hay.includes(` ${query}`)) return 3;
  if (hay.includes(query)) return 2;
  let from = 0;
  for (const ch of query) {
    from = hay.indexOf(ch, from);
    if (from === -1) return 0;
    from += 1;
  }
  return 1;
}

/** The matches as they are shown: by group, best first within each, so the arrow keys follow the page. */
function matchSections(commands: Command[], query: string): Array<{ label: string; verb: string; items: Command[] }> {
  const needle = query.trim().toLowerCase();
  const scored = commands
    .map((command) => ({ command, score: score(command, needle) }))
    .filter((match) => match.score > 0)
    .sort((a, b) => b.score - a.score);
  return GROUPS.map(({ id, label, verb }) => ({
    label,
    verb,
    items: scored.filter((match) => match.command.group === id).map((match) => match.command),
  })).filter((section) => section.items.length > 0);
}

function CommandRow({
  command,
  verb,
  active,
  onHold,
}: {
  command: Command;
  verb: string;
  active: boolean;
  onHold: () => void;
}): JSX.Element {
  const { instance } = command;
  return (
    <button
      type="button"
      id={`cp-cmd-${command.id}`}
      class="cp-cmd-item"
      role="option"
      tabIndex={-1}
      aria-selected={active}
      data-active={active}
      data-sound-silent="true"
      onMouseMove={onHold}
      onClick={() => void command.perform()}
    >
      <span class="cp-cmd-lead">
        {instance ? (
          <InstanceTile inst={instance} />
        ) : (
          command.icon && <Icon name={command.icon} size={16} stroke={1.8} />
        )}
      </span>
      <span class="cp-cmd-name">{command.label}</span>
      {instance && (
        <span class="cp-chips">
          {command.session && <SessionChip session={command.session} />}
          <InstanceFacts inst={instance} />
        </span>
      )}
      <span class="cp-cmd-end">
        {command.keys && <Keys parts={command.keys} />}
        {active && (
          <span class="cp-cmd-verb">
            {verb}
            <Kbd>↵</Kbd>
          </span>
        )}
      </span>
    </button>
  );
}

export function CommandPalette(): JSX.Element | null {
  const open = commandPaletteOpen.value;
  const [query, setQuery] = useState('');
  const [activeId, setActiveId] = useState<string | null>(null);
  const [more, setMore] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const drag = useDraggableOverlay<HTMLDivElement>({ id: 'command-center', enabled: open });

  const commands = open ? buildCommands() : [];
  const sections = useMemo(() => matchSections(commands, query), [commands, query]);
  const flat = sections.flatMap((section) => section.items);
  const active = flat.find((command) => command.id === activeId) ?? flat[0] ?? null;

  // Opening starts clean and hands focus back to where it was on close.
  useEffect(() => {
    if (!open) return undefined;
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    setQuery('');
    setActiveId(null);
    const frame = window.requestAnimationFrame(() => inputRef.current?.focus());
    return () => {
      window.cancelAnimationFrame(frame);
      if (opener?.isConnected && (!document.activeElement || document.activeElement === document.body)) opener.focus();
    };
  }, [open]);

  useEffect(() => {
    if (!open) return undefined;
    const onKey = (event: KeyboardEvent): void => {
      const step = event.key === 'ArrowDown' ? 1 : event.key === 'ArrowUp' ? -1 : 0;
      if (step !== 0) {
        const at = active ? flat.indexOf(active) : -1;
        const next = flat[Math.max(0, Math.min(flat.length - 1, at + step))];
        if (next) setActiveId(next.id);
      } else if (event.key === 'Enter') {
        if (active) void active.perform();
      } else if (event.key === 'Escape') {
        close();
      } else if (event.key === 'Tab') {
        inputRef.current?.focus();
      } else {
        return;
      }
      event.preventDefault();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [open, active, flat]);

  // Rows past the fold fade out instead of ending on a hard edge.
  const measureMore = (): void => {
    const list = listRef.current;
    if (list) setMore(list.scrollHeight - list.scrollTop - list.clientHeight > 4);
  };
  useLayoutEffect(() => {
    if (active) document.getElementById(`cp-cmd-${active.id}`)?.scrollIntoView({ block: 'nearest' });
    measureMore();
  }, [active?.id, flat.length]);

  if (!open) return null;

  return (
    <div
      class="cp-cmd-overlay"
      onClick={(event) => {
        if (event.target === event.currentTarget) close();
      }}
    >
      <div
        class="cp-cmd cp-pop"
        ref={drag.surfaceRef}
        style={drag.style}
        data-positioned={drag.isPositioned}
        role="dialog"
        aria-modal="true"
        aria-label="Command palette"
      >
        <div class="cp-cmd-search" {...drag.dragHandleProps}>
          <Icon name="search" size={16} />
          <input
            ref={inputRef}
            role="combobox"
            aria-expanded="true"
            aria-controls="cp-cmd-list"
            aria-activedescendant={active ? `cp-cmd-${active.id}` : undefined}
            aria-label="Search or jump to"
            placeholder="Search or jump to"
            autocomplete="off"
            spellcheck={false}
            value={query}
            onInput={(event) => {
              setQuery(event.currentTarget.value);
              setActiveId(null);
            }}
          />
          <Kbd>esc</Kbd>
        </div>
        {flat.length === 0 ? (
          <p class="cp-cmd-none" role="status">
            <Face eyes="closed" size={24} tone="mute" />
            No matches
          </p>
        ) : (
          <>
            <div
              class="cp-cmd-list"
              id="cp-cmd-list"
              ref={listRef}
              role="listbox"
              aria-label="Results"
              data-more={more}
              onScroll={measureMore}
            >
              {sections.map((section) => (
                <div key={section.label} role="group" aria-label={section.label}>
                  <div class="cp-cmd-group">{section.label}</div>
                  {section.items.map((command) => (
                    <CommandRow
                      key={command.id}
                      command={command}
                      verb={section.verb}
                      active={command === active}
                      onHold={() => setActiveId(command.id)}
                    />
                  ))}
                </div>
              ))}
            </div>
            <KeyLegend keys={LEGEND} />
          </>
        )}
      </div>
    </div>
  );
}
