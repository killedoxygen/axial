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

function WorldsEmptyArt(): JSX.Element {
  return (
    <svg class="cp-od-worlds-svg" xmlns="http://www.w3.org/2000/svg" viewBox="0 0 180 172.3" aria-hidden="true">
      <polygon fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="21.3 30.9 34.5 24.3 47.7 30.9 47.7 45.7 34.5 52.3 21.3 45.7" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="21.3 30.9 34.5 37.5 47.7 30.9" />
      <line fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" x1="34.5" x2="34.5" y1="37.5" y2="52.3" />
      <polygon fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="44.3 58.3 57.5 51.7 70.7 58.3 70.7 73.1 57.5 79.7 44.3 73.1" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="44.3 58.3 57.5 64.9 70.7 58.3" />
      <line fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" x1="57.5" x2="57.5" y1="64.9" y2="79.7" />
      <polygon fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="81.0 16.7 90.6 2.3 100.2 16.7 90.6 21.5" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="83.1 17.8 75.6 29.0 90.6 36.5 105.6 29.0 98.1 17.8" />
      <polygon fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="78.1 30.3 70.6 41.5 90.6 51.5 110.6 41.5 103.1 30.3 90.6 36.5" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="86.6 49.5 86.6 56.2 90.6 58.2 94.6 56.2 94.6 49.5" />
      <line fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" x1="90.6" x2="90.6" y1="51.5" y2="58.2" />
      <polygon class="cp-od-worlds-accent" fill="none" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="122.4 29.7 135.6 23.1 148.8 29.7 148.8 44.5 135.6 51.1 122.4 44.5" />
      <polyline class="cp-od-worlds-accent" fill="none" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="122.4 29.7 135.6 36.3 148.8 29.7" />
      <line class="cp-od-worlds-accent" fill="none" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" x1="135.6" x2="135.6" y1="36.3" y2="51.1" />
      <polygon fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="108.9 64.8 122.1 58.2 135.3 64.8 135.3 79.6 122.1 86.2 108.9 79.6" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="108.9 64.8 122.1 71.4 135.3 64.8" />
      <line fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" x1="122.1" x2="122.1" y1="71.4" y2="86.2" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-miterlimit="10" points="28.6 50.1 13.7 55.6 2.5 73 2.5 94.2 8.3 98.3 18.3 126.4 33.7 132.3 51.7 154.9 71.9 148.3 83.9 158.3 95.9 169.8 117 150.6 134.6 147.7 149.6 126.1 161.5 120.5 171.3 96.2 177.9 91 177.8 71.5 167.9 60.2 166.7 54.3 147.3 47.9" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linecap="round" stroke-miterlimit="10" points="2.7 73.3 24.7 87.8 43.4 96 46.7 98.4 68.5 96.5 106.5 102.7 119 95.5 122.5 93.8 152.6 90.4 154.8 87.3 177.8 71.8" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linecap="round" stroke-miterlimit="10" points="8.6 98.5 25.9 107.1 46.6 114.9 55.1 114 75.7 135.9 96.3 119.8 106.5 119.9 106.5 102.7" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linecap="round" stroke-miterlimit="10" points="152.6 90.4 152.6 107.1 144.5 109.9 138 120.5 124.1 129.1 116.9 150.4" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linecap="round" stroke-linejoin="round" stroke-miterlimit="10" points="75.9 135.9 83.9 158.3 84 158.5" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="24.9 88 25.7 107.1 25.8 107.1" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linecap="round" stroke-linejoin="round" stroke-miterlimit="10" points="46.6 98.4 46.5 114.7 46.6 114.9" />
      <line fill="none" stroke="#808184" stroke-width="0.6179" stroke-miterlimit="10" x1="25.9" x2="33.9" y1="107.3" y2="132.3" />
      <line fill="none" stroke="#808184" stroke-width="0.6179" stroke-miterlimit="10" x1="33.9" x2="46.4" y1="132.3" y2="115.1" />
      <polyline fill="none" stroke="#808184" stroke-width="0.6179" stroke-linecap="round" stroke-miterlimit="10" points="106.7 120 124 128.9 118.1 107.3 118.3 95.7" />
      <line fill="none" stroke="#808184" stroke-width="0.6179" stroke-miterlimit="10" x1="106.9" x2="117.9" y1="119.3" y2="107.3" />
      <polyline fill="none" stroke="#808184" stroke-width="0.6179" stroke-linecap="round" stroke-miterlimit="10" points="149.6 125.6 152.4 107.3 171.3 96.4" />
      <path fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linecap="square" stroke-miterlimit="10" d="m50.4 44.4 6.2-1.7 15.4-0.1" />
      <path fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-miterlimit="10" d="m110.9 42.5 8.8 0.7" />
      <polygon fill="none" stroke="#808184" stroke-width="0.6179" stroke-linecap="round" stroke-linejoin="round" stroke-miterlimit="10" points="18.1 68.7 26.3 64.6 34.5 68.7 34.5 71.2 26.3 75.3 18.1 71.2" />
      <polyline fill="none" stroke="#808184" stroke-width="0.6179" stroke-linecap="round" stroke-linejoin="round" stroke-miterlimit="10" points="18.1 68.7 26.3 72.8 34.5 68.7" />
      <line fill="none" stroke="#808184" stroke-width="0.6179" stroke-linecap="round" stroke-linejoin="round" stroke-miterlimit="10" x1="26.3" x2="26.3" y1="72.8" y2="75.3" />
      <polygon fill="none" stroke="#808184" stroke-width="0.6179" stroke-linecap="round" stroke-linejoin="round" stroke-miterlimit="10" points="77.8 82.7 86.0 78.6 94.2 82.7 94.2 85.2 86.0 89.3 77.8 85.2" />
      <polyline fill="none" stroke="#808184" stroke-width="0.6179" stroke-linecap="round" stroke-linejoin="round" stroke-miterlimit="10" points="77.8 82.7 86.0 86.8 94.2 82.7" />
      <line fill="none" stroke="#808184" stroke-width="0.6179" stroke-linecap="round" stroke-linejoin="round" stroke-miterlimit="10" x1="86.0" x2="86.0" y1="86.8" y2="89.3" />
      <polygon fill="none" stroke="#808184" stroke-width="0.6179" stroke-linecap="round" stroke-linejoin="round" stroke-miterlimit="10" points="141.1 63.0 149.3 58.9 157.5 63.0 157.5 65.5 149.3 69.6 141.1 65.5" />
      <polyline fill="none" stroke="#808184" stroke-width="0.6179" stroke-linecap="round" stroke-linejoin="round" stroke-miterlimit="10" points="141.1 63.0 149.3 67.1 157.5 63.0" />
      <line fill="none" stroke="#808184" stroke-width="0.6179" stroke-linecap="round" stroke-linejoin="round" stroke-miterlimit="10" x1="149.3" x2="149.3" y1="67.1" y2="69.6" />
    </svg>
  );
}

function WorldsCard({ inst, onOpenWorlds }: { inst: EnrichedInstance; onOpenWorlds: () => void }): JSX.Element {
  const count = inst.saves_count ?? 0;
  return (
    <Card padding={22} class={`cp-od-worlds-card${count === 0 ? ' cp-od-worlds-card--empty' : ''}`}>
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
            <WorldsEmptyArt />
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
    <Card padding={22}>
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
    <Card padding={16} class="cp-od-logs-card">
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
    <Card padding={20} class="cp-od-quick-card">
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
    <Card padding={22}>
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
    <Card padding={22}>
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
    <Card padding={22}>
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

function LaunchSplitButton({
  inst,
  onLaunch,
  onOpenLogs,
  onOpenSettings,
}: {
  inst: EnrichedInstance;
  onLaunch: () => void;
  onOpenLogs: () => void;
  onOpenSettings: () => void;
}): JSX.Element {
  return (
    <div class="cp-instance-split-launch" role="group" aria-label="Launch actions">
      <button
        class="cp-instance-split-launch-main"
        type="button"
        onClick={onLaunch}
        data-sound="launchPress"
      >
        <Icon name="play" size={18} stroke={1.8} />
        Launch
      </button>
      <button
        class="cp-instance-split-launch-menu"
        type="button"
        aria-label="Launch options"
        aria-haspopup="menu"
        onClick={(e) => openContextMenu(e, [
          { icon: 'play', label: 'Launch now', onSelect: onLaunch },
          { icon: 'settings', label: 'Launch settings', onSelect: onOpenSettings },
          { icon: 'terminal', label: 'View launch logs', onSelect: onOpenLogs },
          { label: '', onSelect: () => {}, divider: true },
          { icon: 'folder', label: 'Open instance folder', onSelect: () => void openInstanceFolder(inst.id) },
        ])}
      >
        <Icon name="chevron-down" size={16} stroke={2.3} />
      </button>
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
                <LaunchSplitButton
                  inst={inst}
                  onLaunch={onPlay}
                  onOpenLogs={() => setTab('logs')}
                  onOpenSettings={() => setTab('settings')}
                />
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
