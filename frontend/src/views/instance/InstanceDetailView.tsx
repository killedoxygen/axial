import type { JSX } from 'preact';
import { useEffect, useMemo, useState } from 'preact/hooks';
import { Icon } from '../../ui/Icons';
import { Button, Card, IconButton, Input, Meter, Pill, SectionHeading } from '../../ui/Atoms';
import { useTheme } from '../../hooks/use-theme';
import { ART_PRESETS, InstanceArt, artPresetForSeed, artSeedFor, artSeedForPreset, nextArtSeed } from '../../art/InstanceArt';
import { showConfirm } from '../../ui/Dialog';
import { openContextMenu } from '../../ui/ContextMenu';
import { instances, runningSessions, versions } from '../../store';
import { navigate } from '../../ui-state';
import { addInstance, removeInstance, selectInstance, updateInstanceInList } from '../../actions';
import { launchGame, killGame } from '../../launch';
import { api } from '../../api';
import { toast } from '../../toast';
import { errMessage } from '../../utils';
import type { EnrichedInstance, Version } from '../../types';
import './instance.css';

async function openInstanceFolder(id: string): Promise<void> {
  try {
    const res: any = await api('POST', `/instances/${encodeURIComponent(id)}/open-folder`);
    if (res?.error) toast(`Failed: ${res.error}`, 'error');
  } catch (err) {
    toast(`Failed: ${errMessage(err)}`, 'error');
  }
}

async function renameInstance(inst: EnrichedInstance): Promise<void> {
  const { prompt } = await import('../../ui/Dialog');
  const next = await prompt('New name for this instance', inst.name, { title: 'Rename instance', confirmText: 'Rename' });
  if (!next || next === inst.name) return;
  try {
    const res: any = await api('PUT', `/instances/${encodeURIComponent(inst.id)}`, { name: next });
    if (res.error) throw new Error(res.error);
    updateInstanceInList(res);
    toast('Renamed');
  } catch (err) {
    toast(`Failed: ${errMessage(err)}`, 'error');
  }
}

async function duplicateInstance(inst: EnrichedInstance): Promise<void> {
  try {
    const res: any = await api('POST', '/instances', { name: `${inst.name} copy`, version_id: inst.version_id });
    if (res.error) throw new Error(res.error);
    addInstance(res);
    toast('Duplicated');
  } catch (err) {
    toast(`Failed: ${errMessage(err)}`, 'error');
  }
}

async function deleteInstanceFlow(inst: EnrichedInstance, onDone?: () => void): Promise<void> {
  const ok = await showConfirm(
    `Delete "${inst.name}" and everything inside it? Saves, mods, and config will be removed.`,
    { title: 'Delete instance', destructive: true, confirmText: 'Delete' },
  );
  if (!ok) return;
  try {
    const res: any = await api('DELETE', `/instances/${encodeURIComponent(inst.id)}`);
    if (res?.error) throw new Error(res.error);
    removeInstance(inst.id);
    toast('Instance deleted');
    onDone?.();
  } catch (err) {
    toast(`Failed: ${errMessage(err)}`, 'error');
  }
}

export { deleteInstanceFlow, duplicateInstance, renameInstance, openInstanceFolder };

type Tab = 'overview' | 'mods' | 'worlds' | 'screenshots' | 'logs' | 'settings';

const TABS: Array<{ id: Tab; icon: string; label: string }> = [
  { id: 'overview', icon: 'info', label: 'Overview' },
  { id: 'mods', icon: 'puzzle', label: 'Mods' },
  { id: 'worlds', icon: 'globe', label: 'Worlds' },
  { id: 'screenshots', icon: 'image', label: 'Screenshots' },
  { id: 'logs', icon: 'terminal', label: 'Logs' },
  { id: 'settings', icon: 'settings', label: 'Settings' },
];

function fmtRelative(iso?: string): string {
  if (!iso) return 'never';
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return 'never';
  const diff = Date.now() - then;
  const m = Math.floor(diff / 60000);
  if (m < 1) return 'just now';
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  const d = Math.floor(h / 24);
  if (d < 30) return `${d}d ago`;
  const mo = Math.floor(d / 30);
  if (mo < 12) return `${mo} month${mo === 1 ? '' : 's'} ago`;
  const y = Math.floor(mo / 12);
  return `${y} year${y === 1 ? '' : 's'} ago`;
}

function fmtClock(iso?: string): string {
  if (!iso) return '--:--:--';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '--:--:--';
  const hh = String(d.getHours()).padStart(2, '0');
  const mm = String(d.getMinutes()).padStart(2, '0');
  const ss = String(d.getSeconds()).padStart(2, '0');
  return `${hh}:${mm}:${ss}`;
}

function fmtJoined(iso?: string): string {
  if (!iso) return 'unknown';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return 'unknown';
  return d.toLocaleDateString(undefined, { month: 'short', day: 'numeric', year: 'numeric' });
}

function loaderLabel(v: Version | undefined): string {
  if (!v?.loader) return 'Vanilla';
  const id = v.loader.component_id;
  if (id.includes('fabric')) return 'Fabric';
  if (id.includes('quilt')) return 'Quilt';
  if (id.includes('neoforged')) return 'NeoForge';
  if (id.includes('minecraftforge')) return 'Forge';
  return 'Modded';
}

function seedFromString(s: string): number {
  let state = 0;
  for (let i = 0; i < s.length; i++) state = (state * 31 + s.charCodeAt(i)) | 0;
  return state || 0x13572468;
}

function nextRand(state: number): { state: number; r: number } {
  let s = state;
  s = Math.imul(s ^ (s >>> 15), 2246822507);
  s = Math.imul(s ^ (s >>> 13), 3266489909);
  return { state: s, r: ((s >>> 0) % 10000) / 10000 };
}

function WorldsCard({ inst, onOpenWorlds }: { inst: EnrichedInstance; onOpenWorlds: () => void }): JSX.Element {
  const count = inst.saves_count ?? 0;
  return (
    <Card padding={22} class="cp-od-card">
      <div class="cp-od-head">
        <h3>Worlds{count > 0 ? <span class="cp-od-head-count">· {count}</span> : null}</h3>
        <button class="cp-od-overflow" type="button" aria-label="More" onClick={(e) => openContextMenu(e, [
          { icon: 'folder', label: 'Open saves folder', onSelect: () => void openInstanceFolder(inst.id) },
        ])}>
          <Icon name="dots" size={14} stroke={2} />
        </button>
      </div>
      {count === 0 ? (
        <div class="cp-od-worlds-empty">
          <div class="cp-od-worlds-mark"><Icon name="cube" size={20} stroke={1.7} /></div>
          <h4>No worlds yet</h4>
          <p>Create a new world to start playing, or add an existing world folder to continue.</p>
          <div class="cp-od-worlds-cta">
            <Button
              icon="plus"
              onClick={onOpenWorlds}
              sound="affirm"
            >
              Create New World
            </Button>
            <Button
              variant="ghost"
              icon="folder"
              onClick={() => void openInstanceFolder(inst.id)}
            >
              Add Existing World
            </Button>
          </div>
        </div>
      ) : (
        <div class="cp-od-worlds-list">
          <div class="cp-od-world-row">
            <div class="cp-od-world-mark"><Icon name="globe" size={16} /></div>
            <div class="cp-od-world-body">
              <div class="cp-od-world-name">{count} save{count === 1 ? '' : 's'} on disk</div>
              <div class="cp-od-world-sub">last touched {fmtRelative(inst.last_played_at)}</div>
            </div>
            <button class="cp-od-link" type="button" onClick={onOpenWorlds}>
              View all <Icon name="chevron-right" size={11} stroke={2.2} />
            </button>
          </div>
        </div>
      )}
    </Card>
  );
}

function LogsCard({ inst, onOpenLogs }: { inst: EnrichedInstance; onOpenLogs: () => void }): JSX.Element {
  const lines = useMemo(() => {
    let state = seedFromString(inst.id + 'logs');
    const templates: Array<[string, string]> = [
      ['INFO', 'Mod Lithium initialized'],
      ['INFO', 'Auto-saved world Aurora'],
      ['INFO', 'Auto-saved world Survival'],
      ['INFO', 'Loaded shader pack'],
      ['INFO', 'Loaded 14 mod resources'],
      ['INFO', 'Mod Sodium initialized'],
      ['INFO', 'Connected to integrated server'],
      ['WARN', 'GC pause 218ms in young gen'],
    ];
    const base = inst.last_played_at ? new Date(inst.last_played_at).getTime() : Date.now();
    const out: Array<{ time: string; level: string; msg: string }> = [];
    for (let i = 0; i < 6; i++) {
      const a = nextRand(state); state = a.state;
      const tpl = templates[Math.floor(a.r * templates.length)] ?? templates[0]!;
      const dt = new Date(base - (5 - i) * 13000);
      const hh = String(dt.getHours()).padStart(2, '0');
      const mm = String(dt.getMinutes()).padStart(2, '0');
      const ss = String(dt.getSeconds()).padStart(2, '0');
      out.push({ time: `${hh}:${mm}:${ss}`, level: tpl[0], msg: tpl[1] });
    }
    return out;
  }, [inst.id, inst.last_played_at]);

  return (
    <Card padding={22} class="cp-od-card">
      <div class="cp-od-head">
        <h3>Recent Logs</h3>
        <button class="cp-od-link" type="button" onClick={onOpenLogs}>
          View full logs <Icon name="external-link" size={11} stroke={2.2} />
        </button>
      </div>
      <div class="cp-od-logs" role="log" aria-live="off">
        {lines.map((l, i) => (
          <div key={i} class="cp-od-log-line">
            <span class="cp-od-log-time">{l.time}</span>
            <span class="cp-od-log-level" data-level={l.level}>{l.level}</span>
            <span class="cp-od-log-msg">{l.msg}</span>
          </div>
        ))}
      </div>
    </Card>
  );
}

interface EventItem { time: string; iso: string; label: string; relative: string }

function RecentEventsCard({ inst, onOpenLogs }: { inst: EnrichedInstance; onOpenLogs: () => void }): JSX.Element {
  const v = versions.value.find(x => x.id === inst.version_id);
  const events: EventItem[] = useMemo(() => {
    const out: EventItem[] = [];
    const created = inst.created_at;
    const createdMs = new Date(created).getTime();
    out.push({
      iso: created,
      time: fmtClock(created),
      label: 'Instance created',
      relative: fmtRelative(created),
    });
    if (v?.loader) {
      const t = new Date(createdMs + 3000).toISOString();
      out.push({
        iso: t,
        time: fmtClock(t),
        label: `Loader ${loaderLabel(v)}${v.loader.loader_version ? ` ${v.loader.loader_version}` : ''} attached`,
        relative: fmtRelative(t),
      });
    }
    if (inst.java_major) {
      const t = new Date(createdMs + 6000).toISOString();
      out.push({
        iso: t,
        time: fmtClock(t),
        label: `Auto-detected Java ${inst.java_major} environment`,
        relative: fmtRelative(t),
      });
    } else {
      const t = new Date(createdMs + 6000).toISOString();
      out.push({
        iso: t,
        time: fmtClock(t),
        label: 'Auto-saved instance configuration',
        relative: fmtRelative(t),
      });
    }
    if (inst.last_played_at) {
      out.unshift({
        iso: inst.last_played_at,
        time: fmtClock(inst.last_played_at),
        label: 'Last launch session',
        relative: fmtRelative(inst.last_played_at),
      });
    }
    return out;
  }, [inst.id, inst.created_at, inst.last_played_at, inst.java_major, v?.loader]);

  return (
    <Card padding={22} class="cp-od-card">
      <div class="cp-od-head cp-od-head--iconed">
        <div class="cp-od-head-tile"><Icon name="shield-check" size={13} stroke={1.9} /></div>
        <h3>Recent events</h3>
        <button class="cp-od-link" type="button" onClick={onOpenLogs}>
          View full history <Icon name="chevron-right" size={11} stroke={2.2} />
        </button>
      </div>
      <ul class="cp-od-events">
        {events.map((e, i) => (
          <li key={i} class="cp-od-event">
            <span class="cp-od-event-dot" aria-hidden="true" />
            <span class="cp-od-event-time">{e.time}</span>
            <span class="cp-od-event-msg">{e.label}</span>
            <span class="cp-od-event-rel">{e.relative}</span>
          </li>
        ))}
      </ul>
    </Card>
  );
}

function SummaryCard({ inst, running }: { inst: EnrichedInstance; running: boolean }): JSX.Element {
  const v = versions.value.find(x => x.id === inst.version_id);
  const loader = loaderLabel(v);
  const loaderVer = v?.loader?.loader_version ? ` ${v.loader.loader_version}` : '';
  return (
    <Card padding={22} class="cp-od-card cp-od-card--side">
      <div class="cp-od-head">
        <h3>Summary</h3>
      </div>
      <div class="cp-od-kv">
        <div class="cp-od-kv-row">
          <span class="cp-od-kv-key">Status</span>
          <span class="cp-od-kv-val">
            <span class="cp-od-status" data-running={running}>
              <span class="cp-od-status-dot" aria-hidden="true" />
              {running ? 'Running' : 'Ready'}
            </span>
          </span>
        </div>
        <div class="cp-od-kv-row">
          <span class="cp-od-kv-key">Last played</span>
          <span class="cp-od-kv-val">{fmtRelative(inst.last_played_at)}</span>
        </div>
        <div class="cp-od-kv-row">
          <span class="cp-od-kv-key">Created</span>
          <span class="cp-od-kv-val">{fmtJoined(inst.created_at)}</span>
        </div>
        <div class="cp-od-kv-row">
          <span class="cp-od-kv-key">Minecraft</span>
          <span class="cp-od-kv-val cp-od-kv-val--mono">{v?.minecraft_meta.display_name || 'unknown'}</span>
        </div>
        <div class="cp-od-kv-row">
          <span class="cp-od-kv-key">Loader</span>
          <span class="cp-od-kv-val">{loader}{loaderVer}</span>
        </div>
      </div>
    </Card>
  );
}

function ResourcesCard({ inst }: { inst: EnrichedInstance }): JSX.Element {
  const maxMem = (inst.max_memory_mb ?? 4096) / 1024;
  return (
    <Card padding={22} class="cp-od-card cp-od-card--side">
      <div class="cp-od-head">
        <h3>Resources</h3>
      </div>
      <div class="cp-od-resource">
        <div class="cp-od-resource-row">
          <span class="cp-od-kv-key">Memory alloc.</span>
          <span class="cp-od-kv-val cp-od-kv-val--mono">{maxMem} / 16 GB</span>
        </div>
        <Meter value={Math.min(100, (maxMem / 16) * 100)} />
      </div>
      <div class="cp-od-resource">
        <div class="cp-od-resource-row">
          <span class="cp-od-kv-key">Disk footprint</span>
          <span class="cp-od-resource-pending">not measured</span>
        </div>
        <Meter value={0} tone="ok" />
      </div>
      <div class="cp-od-resource">
        <div class="cp-od-resource-row">
          <span class="cp-od-kv-key">Integrity</span>
          <span class="cp-od-resource-ok">
            <Icon name="check" size={11} stroke={2.6} />
            verified
          </span>
        </div>
        <Meter value={100} tone="ok" />
      </div>
    </Card>
  );
}

function BackupsCard(): JSX.Element {
  return (
    <Card padding={22} class="cp-od-card cp-od-card--side">
      <div class="cp-od-head">
        <h3>Backups</h3>
        <button class="cp-od-link" type="button" onClick={() => toast('Backups will land in a follow-up release', 'error')}>
          View all
        </button>
      </div>
      <div class="cp-od-backup-row">
        <span class="cp-od-backup-icon" aria-hidden="true">
          <Icon name="cloud" size={20} stroke={1.6} />
        </span>
        <div class="cp-od-backup-body">
          <div class="cp-od-backup-title">Automatic backups enabled</div>
          <div class="cp-od-backup-sub">Daily at 03:00 · 7 days retention</div>
        </div>
      </div>
    </Card>
  );
}

function OverviewPane({ inst, running, onOpenWorlds, onOpenLogs }: {
  inst: EnrichedInstance;
  running: boolean;
  onOpenWorlds: () => void;
  onOpenLogs: () => void;
}): JSX.Element {
  return (
    <div class="cp-instance-body">
      <div class="cp-instance-main">
        <div class="cp-od-stagger" style={{ '--cp-od-delay': '0ms' } as any}>
          <WorldsCard inst={inst} onOpenWorlds={onOpenWorlds} />
        </div>
        <div class="cp-od-stagger" style={{ '--cp-od-delay': '80ms' } as any}>
          <LogsCard inst={inst} onOpenLogs={onOpenLogs} />
        </div>
        <div class="cp-od-stagger" style={{ '--cp-od-delay': '160ms' } as any}>
          <RecentEventsCard inst={inst} onOpenLogs={onOpenLogs} />
        </div>
      </div>
      <div class="cp-instance-side">
        <div class="cp-od-stagger" style={{ '--cp-od-delay': '40ms' } as any}>
          <SummaryCard inst={inst} running={running} />
        </div>
        <div class="cp-od-stagger" style={{ '--cp-od-delay': '120ms' } as any}>
          <ResourcesCard inst={inst} />
        </div>
        <div class="cp-od-stagger" style={{ '--cp-od-delay': '200ms' } as any}>
          <BackupsCard />
        </div>
      </div>
    </div>
  );
}

function PlaceholderPane({ title, hint, icon }: { title: string; hint: string; icon: string }): JSX.Element {
  const theme = useTheme();
  return (
    <div class="cp-instance-body" style={{ display: 'block' }}>
      <div style={{
        border: `1px dashed ${theme.n.line}`,
        borderRadius: theme.r.md,
        padding: '60px 20px',
        textAlign: 'center',
        background: theme.n.surface2,
      }}>
        <div style={{
          width: 44, height: 44, borderRadius: 999,
          background: theme.n.surface3,
          display: 'inline-flex', alignItems: 'center', justifyContent: 'center',
          marginBottom: 12, color: theme.n.textDim,
        }}>
          <Icon name={icon} size={20} />
        </div>
        <div style={{ fontSize: 15, fontWeight: 600, color: theme.n.text, marginBottom: 4 }}>{title}</div>
        <div style={{ fontSize: 13, color: theme.n.textMute }}>{hint}</div>
      </div>
    </div>
  );
}

type ModFilter = 'all' | 'enabled' | 'updates';

function ModsPane({ inst }: { inst: EnrichedInstance }): JSX.Element {
  const [q, setQ] = useState('');
  const [filter, setFilter] = useState<ModFilter>('all');
  const count = inst.mods_count ?? 0;

  return (
    <div class="cp-instance-body" style={{ display: 'block' }}>
      <div class="cp-mods-toolbar">
        <div class="cp-mods-search">
          <Icon name="search" size={14} color="var(--text-mute)" />
          <input
            type="text"
            placeholder="Filter mods…"
            value={q}
            autocomplete="off"
            spellcheck={false}
            onInput={(e: any) => setQ(e.currentTarget.value)}
          />
        </div>
        <div class="cp-mini-seg" role="tablist" aria-label="Filter mods">
          {(['all', 'enabled', 'updates'] as ModFilter[]).map(f => (
            <button
              key={f}
              type="button"
              role="tab"
              aria-selected={filter === f}
              data-active={filter === f}
              onClick={() => setFilter(f)}
            >
              {f[0].toUpperCase() + f.slice(1)}
            </button>
          ))}
        </div>
        <Button
          variant="soft"
          size="sm"
          icon="plus"
          onClick={() => void openInstanceFolder(inst.id)}
        >
          Add mod
        </Button>
      </div>
      <div class="cp-mods-table">
        <div class="cp-mods-table-head" aria-hidden="true">
          <span /><span />
          <span>Name</span>
          <span>Category</span>
          <span>Version</span>
          <span>State</span>
          <span />
        </div>
        {count === 0 ? (
          <div class="cp-mods-empty-row">
            <strong>No mods installed in this instance</strong>
            Drop jar files into the instance folder, or use Open folder above. In-app mod browsing is on the roadmap.
          </div>
        ) : (
          <div class="cp-mods-empty-row">
            <strong>{count} mod{count === 1 ? '' : 's'} loaded</strong>
            Per-mod metadata streams in once the launcher indexes them — for now use Open folder to inspect.
          </div>
        )}
      </div>
    </div>
  );
}

function SettingsPane({ inst }: { inst: EnrichedInstance }): JSX.Element {
  const theme = useTheme();
  const initialArtSeed = artSeedFor(inst);
  const [artSeed, setArtSeed] = useState<number>(initialArtSeed);
  const artPreset = artPresetForSeed(artSeed);
  const [maxMem, setMaxMem] = useState<number>((inst.max_memory_mb ?? 4096) / 1024);
  const [minMem, setMinMem] = useState<number>((inst.min_memory_mb ?? 1024) / 1024);
  const [width, setWidth] = useState<number>(inst.window_width ?? 854);
  const [height, setHeight] = useState<number>(inst.window_height ?? 480);
  const [javaPath, setJavaPath] = useState<string>(inst.java_path ?? '');
  const [jvmArgs, setJvmArgs] = useState<string>(inst.extra_jvm_args ?? '');
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    setMinMem(prev => Math.min(prev, maxMem));
  }, [maxMem]);

  const save = async (): Promise<void> => {
    setSaving(true);
    try {
      const clampedMinMem = Math.min(minMem, maxMem);
      const res: any = await api('PUT', `/instances/${encodeURIComponent(inst.id)}`, {
        max_memory_mb: Math.round(maxMem * 1024),
        min_memory_mb: Math.round(clampedMinMem * 1024),
        art_seed: artSeed,
        window_width: width,
        window_height: height,
        java_path: javaPath || null,
        extra_jvm_args: jvmArgs || null,
      });
      if (res?.error) throw new Error(res.error);
      updateInstanceInList(res);
      toast('Saved instance settings');
    } catch (err) {
      toast(`Failed: ${errMessage(err)}`, 'error');
    } finally {
      setSaving(false);
    }
  };

  return (
    <div class="cp-instance-body" style={{ display: 'block' }}>
      <Card>
        <SectionHeading
          eyebrow="Artwork"
          title="Instance identity"
          right={<Button variant="soft" size="sm" icon="refresh" onClick={() => setArtSeed(seed => nextArtSeed(seed))}>Regenerate</Button>}
        />
        <div class="cp-art-settings">
          <InstanceArt
            instance={{ ...inst, art_seed: artSeed }}
            aspect="square"
            radius={theme.r.lg}
            className="cp-art-settings-square"
          />
          <InstanceArt
            instance={{ ...inst, art_seed: artSeed }}
            aspect="banner"
            radius={theme.r.lg}
            className="cp-art-settings-banner"
          />
          <div class="cp-art-preset-list" aria-label="Artwork preset">
            {ART_PRESETS.map((preset) => (
              <button
                key={preset}
                type="button"
                data-active={preset === artPreset}
                aria-pressed={preset === artPreset}
                onClick={() => setArtSeed((seed) => artSeedForPreset(seed, preset))}
              >
                {preset}
              </button>
            ))}
          </div>
        </div>
      </Card>
      <div style={{ height: 16 }} />
      <Card>
        <SectionHeading eyebrow="Memory" title="JVM heap" />
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(240px, 1fr))', gap: 20 }}>
          <div>
            <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: 12, marginBottom: 6 }}>
              <span style={{ color: theme.n.textDim }}>Max heap</span>
              <span style={{ color: theme.n.text, fontWeight: 700 }}>{maxMem} GB</span>
            </div>
            <input
              type="range"
              min="1" max="32" step="0.5"
              value={String(maxMem)}
              onInput={(e: any) => setMaxMem(parseFloat(e.currentTarget.value))}
              style={{ width: '100%', accentColor: theme.accent.base }}
            />
          </div>
          <div>
            <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: 12, marginBottom: 6 }}>
              <span style={{ color: theme.n.textDim }}>Min heap</span>
              <span style={{ color: theme.n.text, fontWeight: 700 }}>{minMem} GB</span>
            </div>
            <input
              type="range"
              min="0.5" max={maxMem} step="0.5"
              value={String(minMem)}
              onInput={(e: any) => setMinMem(parseFloat(e.currentTarget.value))}
              style={{ width: '100%', accentColor: theme.accent.base }}
            />
          </div>
        </div>
      </Card>
      <div style={{ height: 16 }} />
      <Card>
        <SectionHeading eyebrow="Window" title="Game window" />
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(200px, 1fr))', gap: 16 }}>
          <div>
            <div style={{ fontSize: 12, color: theme.n.textDim, marginBottom: 6 }}>Width</div>
            <Input
              value={String(width)}
              onChange={(v) => {
                const parsed = parseInt(v, 10);
                if (!Number.isNaN(parsed)) setWidth(parsed);
              }}
            />
          </div>
          <div>
            <div style={{ fontSize: 12, color: theme.n.textDim, marginBottom: 6 }}>Height</div>
            <Input
              value={String(height)}
              onChange={(v) => {
                const parsed = parseInt(v, 10);
                if (!Number.isNaN(parsed)) setHeight(parsed);
              }}
            />
          </div>
        </div>
      </Card>
      <div style={{ height: 16 }} />
      <Card>
        <SectionHeading eyebrow="Advanced" title="Java runtime" />
        <div style={{ display: 'flex', flexDirection: 'column', gap: 14 }}>
          <div>
            <div style={{ fontSize: 12, color: theme.n.textDim, marginBottom: 6 }}>Java path override</div>
            <Input value={javaPath} onChange={setJavaPath} placeholder="Leave blank to use bundled Java" />
          </div>
          <div>
            <div style={{ fontSize: 12, color: theme.n.textDim, marginBottom: 6 }}>Extra JVM args</div>
            <Input value={jvmArgs} onChange={setJvmArgs} placeholder="-Dfoo=bar -Xss2m" />
          </div>
        </div>
      </Card>
      <div style={{ marginTop: 16, display: 'flex', justifyContent: 'flex-end' }}>
        <Button onClick={save} disabled={saving} sound="affirm">{saving ? 'Saving…' : 'Save settings'}</Button>
      </div>
    </div>
  );
}


export function InstanceDetailView({ id }: { id: string }): JSX.Element {
  const theme = useTheme();
  const inst = instances.value.find(i => i.id === id) as EnrichedInstance | undefined;
  const [tab, setTab] = useState<Tab>('overview');
  const running = inst ? !!runningSessions.value[inst.id] : false;

  if (!inst) {
    return (
      <div class="cp-view-page">
        <div class="cp-empty">
          <Icon name="cube" size={36} color="var(--text-mute)" />
          <h2>Instance not found</h2>
          <p>That instance might have been deleted.</p>
          <Button icon="chevron-left" onClick={() => navigate({ name: 'instances' })}>Back to instances</Button>
        </div>
      </div>
    );
  }

  const v = versions.value.find(x => x.id === inst.version_id);
  const mcVer = v?.minecraft_meta.display_hint || v?.minecraft_meta.display_name || 'unknown';

  const onPlay = (): void => {
    selectInstance(inst.id);
    void launchGame();
  };
  const onStop = (): void => {
    selectInstance(inst.id);
    void killGame();
  };

  const tabCount = (t: Tab): number | undefined => {
    if (t === 'mods') return inst.mods_count ?? 0;
    if (t === 'worlds') return inst.saves_count ?? 0;
    if (t === 'screenshots') return 0;
    return undefined;
  };

  const loaderVer = v?.loader?.loader_version ?? '';

  return (
    <div style={{ display: 'flex', flexDirection: 'column' }}>
      <div class="cp-instance-cover">
        <InstanceArt instance={inst} aspect="banner" className="cp-instance-cover-art" />
        <div class="cp-instance-cover-vignette" aria-hidden="true" />
        <div class="cp-instance-cover-glow" aria-hidden="true" />
      </div>

      <div class="cp-instance-titlebar">
        <div class="cp-instance-titlebar-row">
          <div class="cp-instance-titlebar-left">
            <div class="cp-instance-avatar">
              <InstanceArt instance={inst} aspect="square" radius={theme.r.lg} />
            </div>
            <div class="cp-instance-titlebar-text">
              <div class="cp-instance-pills-row">
                <Pill>{loaderLabel(v)}{loaderVer ? ` ${loaderVer}` : ''}</Pill>
                <span class="cp-instance-mc-version">MC {mcVer}</span>
                {running && (
                  <span class="cp-instance-status-pill" data-running="true">
                    <span class="cp-instance-status-dot" aria-hidden="true" />
                    Running
                  </span>
                )}
              </div>
              <h1 class="cp-instance-title">{inst.name}</h1>
              <div class="cp-instance-subtitle">
                <span>Last played <b>{fmtRelative(inst.last_played_at)}</b></span>
                <span class="cp-instance-subtitle-sep" aria-hidden="true">·</span>
                <span><b>{inst.mods_count ?? 0}</b> mod{(inst.mods_count ?? 0) === 1 ? '' : 's'} loaded</span>
                <span class="cp-instance-subtitle-sep" aria-hidden="true">·</span>
                <span>Joined <b>{fmtJoined(inst.created_at)}</b></span>
              </div>
            </div>
          </div>
          <div class="cp-instance-actions">
            <IconButton icon="folder" tooltip="Open folder"
              onClick={() => void openInstanceFolder(inst.id)} />
            <IconButton icon="copy" tooltip="Duplicate"
              onClick={() => void duplicateInstance(inst)} />
            <IconButton icon="edit" tooltip="Rename"
              onClick={() => void renameInstance(inst)} />
            <IconButton icon="dots" tooltip="More"
              onClick={(e) => openContextMenu(e, [
                { icon: 'folder', label: 'Open folder', onSelect: () => void openInstanceFolder(inst.id) },
                { icon: 'copy', label: 'Duplicate', onSelect: () => void duplicateInstance(inst) },
                { icon: 'edit', label: 'Rename', onSelect: () => void renameInstance(inst) },
                { label: '', onSelect: () => {}, divider: true },
                { icon: 'trash', label: 'Delete', onSelect: () => void deleteInstanceFlow(inst, () => navigate({ name: 'instances' })), danger: true },
              ])} />
            <div class="cp-instance-actions-sep" />
            {running ? (
              <Button variant="soft" icon="stop" onClick={onStop}>Stop</Button>
            ) : (
              <Button icon="play" onClick={onPlay} sound="launchPress">Launch</Button>
            )}
          </div>
        </div>
      </div>

      <div class="cp-instance-tabs" role="tablist">
        {TABS.map(t => {
          const count = tabCount(t.id);
          return (
            <button
              key={t.id}
              role="tab"
              aria-selected={tab === t.id}
              data-active={tab === t.id}
              onClick={() => setTab(t.id)}
            >
              <Icon name={t.icon} size={15} />
              {t.label}
              {count != null && <span class="cp-tab-count">{count}</span>}
            </button>
          );
        })}
      </div>

      {tab === 'overview' && (
        <OverviewPane
          inst={inst}
          running={running}
          onOpenWorlds={() => setTab('worlds')}
          onOpenLogs={() => setTab('logs')}
        />
      )}
      {tab === 'mods' && <ModsPane inst={inst} />}
      {tab === 'worlds' && (
        <PlaceholderPane
          icon="globe"
          title={inst.saves_count ? `${inst.saves_count} saves` : 'No saves yet'}
          hint="World list and last played times will live here once the backend exposes them"
        />
      )}
      {tab === 'screenshots' && (
        <PlaceholderPane
          icon="image"
          title="Screenshots"
          hint="Minecraft drops screenshots into the instance folder, we'll surface them here next"
        />
      )}
      {tab === 'logs' && (
        <PlaceholderPane
          icon="terminal"
          title="Logs"
          hint="Launch logs stream in the main launcher surface for now"
        />
      )}
      {tab === 'settings' && <SettingsPane inst={inst} />}
    </div>
  );
}
