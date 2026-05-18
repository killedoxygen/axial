import type { JSX } from 'preact';
import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { Icon } from '../../ui/Icons';
import { Button, Card, IconButton, Input, Pill, SectionHeading } from '../../ui/Atoms';
import { Slider, type SliderZone } from '../../ui/Slider';
import { useTheme } from '../../hooks/use-theme';
import { ART_PRESETS, InstanceArt, artPresetForSeed, artSeedFor, artSeedForPreset, nextArtSeed } from '../../art/InstanceArt';
import { showConfirm } from '../../ui/Dialog';
import { openContextMenu } from '../../ui/ContextMenu';
import { config, instances, runningSessions, systemInfo, versions } from '../../store';
import { navigate } from '../../ui-state';
import { addInstance, removeInstance, selectInstance, updateInstanceInList } from '../../actions';
import { launchGame, killGame } from '../../launch';
import { api } from '../../api';
import { toast } from '../../toast';
import { errMessage, fmtMem, getMemoryRecommendation } from '../../utils';
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

// ─── Worlds — main column, primary content ───────────────────────────────

function WorldsCard({ inst, onOpenWorlds }: { inst: EnrichedInstance; onOpenWorlds: () => void }): JSX.Element {
  const count = inst.saves_count ?? 0;
  return (
    <Card padding={22} class={`cp-od-card cp-od-worlds-card${count === 0 ? ' cp-od-worlds-card--empty' : ''}`}>
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
          <div class="cp-od-worlds-art" aria-hidden="true">
            <img src="worlds_empty.svg" alt="" loading="lazy" draggable={false} />
          </div>
          <div class="cp-od-worlds-lead">
            <div class="cp-od-worlds-copy">
              <h4>No worlds yet</h4>
              <p>Create a new world, import an existing save,<br />or launch Minecraft and create one there.</p>
            </div>
          </div>
          <div class="cp-od-worlds-cta">
            <Button icon="plus" onClick={onOpenWorlds} sound="affirm">Create world</Button>
            <Button variant="ghost" icon="folder" onClick={() => void openInstanceFolder(inst.id)}>Import world</Button>
          </div>
        </div>
      ) : (
        <div class="cp-od-worlds-list">
          <div class="cp-od-world-row">
            <div class="cp-od-world-mark"><Icon name="globe" size={16} /></div>
            <div class="cp-od-world-body">
              <div class="cp-od-world-name">{count} save{count === 1 ? '' : 's'} on disk</div>
              <div class="cp-od-world-sub">Last touched {fmtRelative(inst.last_played_at)}</div>
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

// ─── Activity — replaces "Recent events"; small, human-readable ──────────

interface ActivityItem { label: string; relative: string }

function ActivityCard({ inst, onOpenLogs }: { inst: EnrichedInstance; onOpenLogs: () => void }): JSX.Element {
  const v = versions.value.find(x => x.id === inst.version_id);
  const events: ActivityItem[] = useMemo(() => {
    const out: ActivityItem[] = [];
    const createdMs = new Date(inst.created_at).getTime();
    out.push({ label: 'Instance created', relative: fmtRelative(inst.created_at) });
    if (v?.loader) {
      const t = new Date(createdMs + 3000).toISOString();
      out.push({
        label: `Loader ${loaderLabel(v)}${v.loader.loader_version ? ` ${v.loader.loader_version}` : ''} attached`,
        relative: fmtRelative(t),
      });
    }
    if (inst.java_major) {
      const t = new Date(createdMs + 6000).toISOString();
      out.push({ label: `Java ${inst.java_major} environment detected`, relative: fmtRelative(t) });
    }
    if (inst.last_played_at) {
      out.unshift({ label: 'Last launch session', relative: fmtRelative(inst.last_played_at) });
    }
    return out.slice(0, 3);
  }, [inst.id, inst.created_at, inst.last_played_at, inst.java_major, v?.loader]);

  return (
    <Card padding={22} class="cp-od-card">
      <div class="cp-od-head cp-od-head--iconed">
        <div class="cp-od-head-tile"><Icon name="activity" size={13} stroke={1.9} /></div>
        <h3>Activity</h3>
        <button class="cp-od-link" type="button" onClick={onOpenLogs}>
          View all <Icon name="chevron-right" size={11} stroke={2.2} />
        </button>
      </div>
      <ul class="cp-od-events">
        {events.map((e, i) => (
          <li key={i} class="cp-od-event">
            <span class="cp-od-event-dot" aria-hidden="true" />
            <span class="cp-od-event-msg">{e.label}</span>
            <span class="cp-od-event-rel">{e.relative}</span>
          </li>
        ))}
      </ul>
    </Card>
  );
}

// ─── Logs — demoted to a compact card at the bottom of the main column ──

function LogsCard({ inst, onOpenLogs }: { inst: EnrichedInstance; onOpenLogs: () => void }): JSX.Element {
  const summary = inst.last_played_at ? 'Last launch · no errors' : 'No launch logs yet';
  return (
    <Card padding={16} class="cp-od-card cp-od-logs-card">
      <div class="cp-od-logs-summary">
        <span class="cp-od-logs-icon"><Icon name="terminal" size={14} stroke={1.9} /></span>
        <div class="cp-od-logs-line">
          <strong>Logs</strong>
          <span class="cp-od-logs-sub">{summary}</span>
        </div>
        <button class="cp-od-link" type="button" onClick={onOpenLogs}>
          View logs <Icon name="chevron-right" size={11} stroke={2.2} />
        </button>
      </div>
    </Card>
  );
}

function QuickActionsCard({
  running,
  onLaunch,
  onStop,
  onOpenLogs,
}: {
  running: boolean;
  onLaunch: () => void;
  onStop: () => void;
  onOpenLogs: () => void;
}): JSX.Element {
  return (
    <Card padding={20} class="cp-od-card cp-od-quick-card">
      <div class="cp-od-head">
        <h3>Quick actions</h3>
      </div>
      <div class="cp-od-quick-grid">
        <button
          class="cp-od-quick-action"
          type="button"
          onClick={() => toast('Manual backups will land in a follow-up release')}
        >
          <span class="cp-od-quick-icon"><Icon name="archive" size={15} stroke={1.9} /></span>
          <span class="cp-od-quick-copy">
            <strong>Backup world</strong>
            <span>Create a manual backup</span>
          </span>
        </button>
        <button
          class="cp-od-quick-action"
          type="button"
          disabled={!running}
          onClick={() => {
            onStop();
            window.setTimeout(onLaunch, 450);
          }}
        >
          <span class="cp-od-quick-icon"><Icon name="refresh" size={15} stroke={1.9} /></span>
          <span class="cp-od-quick-copy">
            <strong>Restart</strong>
            <span>{running ? 'Restart the instance' : 'Available while running'}</span>
          </span>
        </button>
        <button
          class="cp-od-quick-action"
          type="button"
          data-tone="danger"
          disabled={!running}
          onClick={onStop}
        >
          <span class="cp-od-quick-icon"><Icon name="stop" size={15} stroke={1.9} /></span>
          <span class="cp-od-quick-copy">
            <strong>Stop</strong>
            <span>{running ? 'Stop the instance' : 'Not running'}</span>
          </span>
        </button>
        <button class="cp-od-quick-action" type="button" onClick={onOpenLogs}>
          <span class="cp-od-quick-icon"><Icon name="terminal" size={15} stroke={1.9} /></span>
          <span class="cp-od-quick-copy">
            <strong>Open logs</strong>
            <span>Inspect launch output</span>
          </span>
        </button>
      </div>
    </Card>
  );
}

// ─── Performance — main-column quick-control card.
// RAM allocation + preset + Java runtime. The slider writes on commit so
// instance launch policy sees the same value after reboot. ─────────────

type Preset = 'low' | 'balanced' | 'high' | 'custom';

interface MemoryPreset {
  id: Exclude<Preset, 'custom'>;
  value: number;
}

interface MemoryProfile {
  max: number;
  recommended: [number, number];
  presets: MemoryPreset[];
}

function roundHalf(value: number): number {
  return Math.round(value * 2) / 2;
}

function memoryGb(valueMb: number | undefined, fallbackMb: number): number {
  const mb = typeof valueMb === 'number' && valueMb > 0 ? valueMb : fallbackMb;
  return Math.max(0.5, mb / 1024);
}

function clampMemoryGb(value: number, min: number, max: number): number {
  return roundHalf(Math.max(min, Math.min(max, value)));
}

function systemMemoryMaxGb(savedGb: number, minGb: number): number {
  const sys = systemInfo.value;
  const detected = sys?.max_allocatable_gb || (sys?.total_memory_mb ? Math.floor(sys.total_memory_mb / 1024) : 16);
  return Math.max(minGb, detected, Math.ceil(savedGb));
}

function systemMemoryRecommendationGb(totalGb: number, minGb: number): [number, number] {
  const sys = systemInfo.value;
  if (sys?.recommended_min_mb && sys.recommended_max_mb) {
    const min = Math.max(minGb, roundHalf(sys.recommended_min_mb / 1024));
    const max = Math.max(min, sys.recommended_max_mb / 1024);
    return [min, clampMemoryGb(max, min, totalGb)];
  }
  const rec = getMemoryRecommendation(totalGb);
  const low = Math.min(Math.max(minGb, rec.rec - 2), totalGb);
  const high = Math.min(totalGb, Math.max(low, rec.rec + 2));
  return [low, high];
}

function memoryProfileForInstance(inst: EnrichedInstance, savedGb: number, minGb: number): MemoryProfile {
  const max = systemMemoryMaxGb(savedGb, minGb);
  const recommended = systemMemoryRecommendationGb(max, minGb);
  const [recMin, recMax] = recommended;
  const modWeight = Math.min(4, Math.floor((inst.mods_count ?? 0) / 50));
  const loaderWeight = versions.value.find(v => v.id === inst.version_id)?.loader ? 1 : 0;
  const headroom = Math.max(0, max - minGb);
  const estimatedSweetSpot = clampMemoryGb(
    Math.max(((recMin + recMax) / 2) + loaderWeight + modWeight, recMin + (headroom * 0.2)),
    minGb,
    max,
  );
  const highHeadroom = Math.max(2, Math.min(8, Math.round(max * 0.12)));
  const high = clampMemoryGb(Math.max(recMax, estimatedSweetSpot + highHeadroom, recMin + (headroom * 0.45)), minGb, max);

  return {
    max,
    recommended,
    presets: [
      { id: 'low', value: clampMemoryGb(recMin, minGb, max) },
      { id: 'balanced', value: estimatedSweetSpot },
      { id: 'high', value: high },
    ],
  };
}

function inferPreset(maxMem: number, presets: MemoryPreset[]): Preset {
  return presets.find(preset => preset.value === maxMem)?.id ?? 'custom';
}

function PerformanceCard({ inst, onOpenSettings }: { inst: EnrichedInstance; onOpenSettings: () => void }): JSX.Element {
  const RAM_MIN = 2;
  const saved = memoryGb(inst.max_memory_mb, config.value?.max_memory_mb ?? 4096);
  const memoryProfile = memoryProfileForInstance(inst, saved, RAM_MIN);
  const ramMax = memoryProfile.max;
  const [recMin, recMax] = memoryProfile.recommended;
  const initialMem = clampMemoryGb(saved, RAM_MIN, ramMax);
  const [maxMem, setMaxMem] = useState<number>(initialMem);
  const [saving, setSaving] = useState(false);
  const savedRef = useRef(initialMem);
  const saveRequestRef = useRef(0);
  const saveTimerRef = useRef<number | null>(null);

  useEffect(() => {
    // If the persisted value changes (PUT elsewhere), realign local state.
    const nextSaved = clampMemoryGb(saved, RAM_MIN, ramMax);
    if (nextSaved !== savedRef.current) {
      savedRef.current = nextSaved;
      setMaxMem(nextSaved);
    }
  }, [ramMax, saved]);

  useEffect(() => {
    return () => {
      if (saveTimerRef.current !== null) window.clearTimeout(saveTimerRef.current);
    };
  }, []);

  const saveMemory = async (nextMem: number): Promise<void> => {
    const clampedMem = clampMemoryGb(nextMem, RAM_MIN, ramMax);
    if (clampedMem === savedRef.current) return;
    const requestId = saveRequestRef.current + 1;
    saveRequestRef.current = requestId;
    setSaving(true);
    try {
      const res: any = await api('PUT', `/instances/${encodeURIComponent(inst.id)}`, { max_memory_mb: Math.round(clampedMem * 1024) });
      if (res?.error) throw new Error(res.error);
      if (requestId !== saveRequestRef.current) return;
      savedRef.current = clampedMem;
      updateInstanceInList(res);
    } catch (err) {
      if (requestId !== saveRequestRef.current) return;
      toast(`Failed: ${errMessage(err)}`, 'error');
    } finally {
      if (requestId === saveRequestRef.current) setSaving(false);
    }
  };

  const scheduleSaveMemory = (nextMem: number): void => {
    const clampedMem = clampMemoryGb(nextMem, RAM_MIN, ramMax);
    setMaxMem(clampedMem);
    if (saveTimerRef.current !== null) window.clearTimeout(saveTimerRef.current);
    saveTimerRef.current = window.setTimeout(() => {
      saveTimerRef.current = null;
      void saveMemory(clampedMem);
    }, 350);
  };

  const commitMemory = (nextMem: number): void => {
    if (saveTimerRef.current !== null) {
      window.clearTimeout(saveTimerRef.current);
      saveTimerRef.current = null;
    }
    void saveMemory(nextMem);
  };

  const preset = inferPreset(maxMem, memoryProfile.presets);
  const highStart = Math.min(ramMax, Math.max(recMax, Math.round(ramMax * 0.75)));
  const memoryZones: SliderZone[] = [
    { from: RAM_MIN, to: recMin, tone: 'low', label: 'Low' },
    { from: recMin, to: recMax, tone: 'sweet', label: 'Sweet spot' },
    { from: recMax, to: highStart, tone: 'high', label: 'High' },
    { from: highStart, to: ramMax, tone: 'extreme', label: 'Extreme' },
  ];

  return (
    <Card padding={22} class="cp-od-card">
      <div class="cp-od-head">
        <h3>Performance</h3>
        <button class="cp-od-link" type="button" onClick={onOpenSettings}>
          Advanced <Icon name="chevron-right" size={11} stroke={2.2} />
        </button>
      </div>

      <div class="cp-od-perf-row">
        <span class="cp-od-perf-key">Memory allocation</span>
        <span class="cp-od-perf-val" aria-live="polite">{fmtMem(maxMem)}</span>
      </div>
      <div class="cp-od-perf-slider">
        <Slider
          value={maxMem}
          min={RAM_MIN}
          max={ramMax}
          step={0.5}
          zones={memoryZones}
          sound="memory"
          onChange={scheduleSaveMemory}
          onCommit={commitMemory}
          ariaLabel="Memory allocation in gigabytes"
        />
      </div>

      <div class="cp-od-perf-preset-row">
        <span class="cp-od-perf-key">Preset</span>
        <div class="cp-mini-seg" role="radiogroup" aria-label="Performance preset">
          {memoryProfile.presets.map(p => (
            <button
              key={p.id}
              type="button"
              role="radio"
              aria-checked={preset === p.id}
              data-active={preset === p.id}
              onClick={() => {
                const next = p.value;
                setMaxMem(next);
                commitMemory(next);
              }}
            >
              {p.id[0].toUpperCase() + p.id.slice(1)}
            </button>
          ))}
        </div>
      </div>

      <div class="cp-od-perf-runtime">
        <span class="cp-od-perf-runtime-mark"><Icon name="check" size={12} stroke={2.6} /></span>
        <span class="cp-od-perf-runtime-text">{inst.java_major ? `Java ${inst.java_major} detected` : 'Auto-detect Java runtime'}</span>
        <button class="cp-od-link" type="button" onClick={onOpenSettings}>Change</button>
      </div>
    </Card>
  );
}

// ─── Maintenance — right rail, single compact list. Backups + Integrity
// + Disk. Healthy states stay quiet. ────────────────────────────────────

function MaintenanceCard(): JSX.Element {
  return (
    <Card padding={22} class="cp-od-card cp-od-card--side">
      <div class="cp-od-head">
        <h3>Maintenance</h3>
      </div>
      <ul class="cp-od-maint-list">
        <li class="cp-od-maint-row">
          <span class="cp-od-maint-icon" data-tone="ok"><Icon name="archive" size={14} stroke={1.8} /></span>
          <div class="cp-od-maint-body">
            <div class="cp-od-maint-title">Backups enabled</div>
            <div class="cp-od-maint-sub">Daily at 03:00 · 7 day retention</div>
          </div>
          <button class="cp-od-link" type="button" onClick={() => toast('Backups will land in a follow-up release')}>Manage</button>
        </li>
        <li class="cp-od-maint-row">
          <span class="cp-od-maint-icon" data-tone="ok"><Icon name="shield-check" size={14} stroke={1.8} /></span>
          <div class="cp-od-maint-body">
            <div class="cp-od-maint-title">Integrity verified</div>
          </div>
          <button class="cp-od-link" type="button" onClick={() => toast('Integrity recheck is queued')}>Verify</button>
        </li>
        <li class="cp-od-maint-row">
          <span class="cp-od-maint-icon" data-tone="mute"><Icon name="archive" size={14} stroke={1.8} /></span>
          <div class="cp-od-maint-body">
            <div class="cp-od-maint-title">Disk usage</div>
            <div class="cp-od-maint-sub">Not measured</div>
          </div>
          <button class="cp-od-link" type="button" onClick={() => toast('Disk measurement will land in a follow-up release')}>Measure</button>
        </li>
      </ul>
    </Card>
  );
}

// ─── Details — quiet glanceable KV; duplicates header on purpose. ──────

function DetailsCard({ inst, running }: { inst: EnrichedInstance; running: boolean }): JSX.Element {
  const v = versions.value.find(x => x.id === inst.version_id);
  const loader = loaderLabel(v);
  const loaderVer = v?.loader?.loader_version ? ` ${v.loader.loader_version}` : '';
  const mcVer = v?.minecraft_meta.display_name || v?.minecraft_meta.display_hint || 'unknown';
  return (
    <Card padding={22} class="cp-od-card cp-od-card--side">
      <div class="cp-od-head">
        <h3>Details</h3>
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
          <span class="cp-od-kv-key">Minecraft</span>
          <span class="cp-od-kv-val cp-od-kv-val--mono">{mcVer}</span>
        </div>
        <div class="cp-od-kv-row">
          <span class="cp-od-kv-key">Loader</span>
          <span class="cp-od-kv-val">{loader}{loaderVer}</span>
        </div>
        <div class="cp-od-kv-row">
          <span class="cp-od-kv-key">Created</span>
          <span class="cp-od-kv-val">{fmtJoined(inst.created_at)}</span>
        </div>
        <div class="cp-od-kv-row">
          <span class="cp-od-kv-key">Last played</span>
          <span class="cp-od-kv-val">{fmtRelative(inst.last_played_at)}</span>
        </div>
      </div>
    </Card>
  );
}

// ─── Overview pane — original bento, Play replaces Summary ──────────────

function OverviewPane({ inst, running, onLaunch, onStop, onOpenWorlds, onOpenLogs, onOpenSettings }: {
  inst: EnrichedInstance;
  running: boolean;
  onLaunch: () => void;
  onStop: () => void;
  onOpenWorlds: () => void;
  onOpenLogs: () => void;
  onOpenSettings: () => void;
}): JSX.Element {
  return (
    <div class="cp-instance-body">
      <div class="cp-instance-main">
        <div class="cp-od-stagger cp-od-worlds-slot" style={{ '--cp-od-delay': '0ms' } as any}>
          <WorldsCard inst={inst} onOpenWorlds={onOpenWorlds} />
        </div>
        <div class="cp-od-stagger" style={{ '--cp-od-delay': '80ms' } as any}>
          <PerformanceCard inst={inst} onOpenSettings={onOpenSettings} />
        </div>
        <div class="cp-od-stagger" style={{ '--cp-od-delay': '160ms' } as any}>
          <QuickActionsCard
            running={running}
            onLaunch={onLaunch}
            onStop={onStop}
            onOpenLogs={onOpenLogs}
          />
        </div>
      </div>
      <div class="cp-instance-side">
        <div class="cp-od-stagger" style={{ '--cp-od-delay': '40ms' } as any}>
          <ActivityCard inst={inst} onOpenLogs={onOpenLogs} />
        </div>
        <div class="cp-od-stagger" style={{ '--cp-od-delay': '120ms' } as any}>
          <MaintenanceCard />
        </div>
        <div class="cp-od-stagger" style={{ '--cp-od-delay': '200ms' } as any}>
          <DetailsCard inst={inst} running={running} />
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
  const [maxMem, setMaxMem] = useState<number>(memoryGb(inst.max_memory_mb, config.value?.max_memory_mb ?? 4096));
  const [minMem, setMinMem] = useState<number>(memoryGb(inst.min_memory_mb, config.value?.min_memory_mb ?? 1024));
  const [width, setWidth] = useState<number>(inst.window_width ?? 854);
  const [height, setHeight] = useState<number>(inst.window_height ?? 480);
  const [javaPath, setJavaPath] = useState<string>(inst.java_path ?? '');
  const [jvmArgs, setJvmArgs] = useState<string>(inst.extra_jvm_args ?? '');
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    setMinMem(prev => Math.min(prev, maxMem));
  }, [maxMem]);

  useEffect(() => {
    setMaxMem(memoryGb(inst.max_memory_mb, config.value?.max_memory_mb ?? 4096));
    setMinMem(memoryGb(inst.min_memory_mb, config.value?.min_memory_mb ?? 1024));
  }, [inst.id, inst.max_memory_mb, inst.min_memory_mb]);

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
    if (t === 'mods') {
      const n = inst.mods_count ?? 0;
      return n > 0 ? n : undefined;
    }
    if (t === 'worlds') {
      const n = inst.saves_count ?? 0;
      return n > 0 ? n : undefined;
    }
    return undefined;
  };

  const loaderVer = v?.loader?.loader_version ?? '';

  return (
    <div class={`cp-instance-page${tab === 'overview' ? ' cp-instance-page--overview' : ''}`}>
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
                <span class="cp-instance-mc-version">Minecraft {mcVer}</span>
              </div>
              <h1 class="cp-instance-title">{inst.name}</h1>
              <div class="cp-instance-subtitle">
                <span>Last played <b>{fmtRelative(inst.last_played_at)}</b></span>
                <span class="cp-instance-subtitle-sep" aria-hidden="true">·</span>
                <span>Created <b>{fmtJoined(inst.created_at)}</b></span>
              </div>
            </div>
          </div>
          <div class="cp-instance-actions">
            <div class="cp-instance-launch">
              {running ? (
                <Button variant="secondary" size="lg" icon="stop" onClick={onStop}>Stop</Button>
              ) : (
                <Button variant="primary" size="lg" icon="play" onClick={onPlay} sound="launchPress">Launch</Button>
              )}
            </div>
            <IconButton icon="folder" tooltip="Open folder"
              onClick={() => void openInstanceFolder(inst.id)} />
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
        <>
          <OverviewPane
            inst={inst}
            running={running}
            onLaunch={onPlay}
            onStop={onStop}
            onOpenWorlds={() => setTab('worlds')}
            onOpenLogs={() => setTab('logs')}
            onOpenSettings={() => setTab('settings')}
          />
          <div class="cp-instance-bottom">
            <LogsCard inst={inst} onOpenLogs={() => setTab('logs')} />
          </div>
        </>
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
