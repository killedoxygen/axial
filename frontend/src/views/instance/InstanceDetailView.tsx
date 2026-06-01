import type { JSX } from 'preact';
import { useCallback, useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { Icon } from '../../ui/Icons';
import { Button, Card, IconButton, Input, Pill } from '../../ui/Atoms';
import { Slider, type SliderZone } from '../../ui/Slider';
import { useTheme } from '../../hooks/use-theme';
import { InstanceArt, artPresetForSeed, artSeedFor, nextArtSeed } from '../../art/InstanceArt';
import { prompt, showChoice } from '../../ui/Dialog';
import { openContextMenu } from '../../ui/ContextMenu';
import { config, installQueue, installState, instances, launchNotices, launchState, runningSessions, systemInfo, versions } from '../../store';
import type { LaunchState } from '../../store';
import { navigate } from '../../ui-state';
import { addInstance, clearLaunchNotice, removeInstance, selectInstance, updateInstanceInList } from '../../actions';
import { launchGame, killGame } from '../../launch';
import { handleInstallClick } from '../../install';
import { api, apiResourceUrl } from '../../api';
import { toast } from '../../toast';
import { errMessage, fmtMem, getMemoryRecommendation } from '../../utils';
import type {
  CompositionTier,
  EnrichedInstance,
  InstancePerformanceMode,
  InstanceLogTail,
  InstanceResourceSummary,
  LaunchNotice,
  LaunchNoticeTone,
  PerformanceHealthResponse,
  PerformanceHealthStatus,
  PerformanceInstanceOperationResponse,
  PerformanceMode,
  PerformanceOperationStatus,
  PerformancePlanResponse,
  Version,
} from '../../types';
import {
  JVM_PRESET_HINTS,
  JVM_PRESET_LABELS,
  JVM_PRESET_ORDER,
  jvmPresetFrom,
  type JvmPreset,
} from '../create/jvm-presets';
import './instance.css';

async function openInstanceFolder(id: string, sub?: string): Promise<void> {
  try {
    const suffix = sub ? `?sub=${encodeURIComponent(sub)}` : '';
    const res: any = await api('POST', `/instances/${encodeURIComponent(id)}/open-folder${suffix}`);
    if (res?.error) toast(`Could not open the instance folder: ${res.error}`, 'error');
  } catch (err) {
    toast(`Could not open the instance folder: ${errMessage(err)}`, 'error');
  }
}

function worldNameError(value: string): string | null {
  const name = value.trim();
  if (!name || name === '.' || name === '..') return 'Use a world name.';
  if (name.startsWith('.')) return 'World names cannot start with a dot.';
  if (/[\\/]/.test(name)) return 'World names cannot include folders.';
  if (/[\u0000-\u001f\u007f]/.test(name)) return 'World names cannot include control characters.';
  return null;
}

async function renameWorld(inst: EnrichedInstance, worldName: string, onDone: () => void): Promise<void> {
  const next = await prompt('New name for this world', worldName, {
    title: 'Rename world',
    confirmText: 'Rename',
    validate: worldNameError,
  });
  const nextName = next?.trim() ?? '';
  if (!nextName || nextName === worldName) return;
  try {
    const res: any = await api('PUT', `/instances/${encodeURIComponent(inst.id)}/worlds/${encodeURIComponent(worldName)}`, { name: nextName });
    if (res?.error) throw new Error(res.error);
    toast('World renamed');
    onDone();
  } catch (err) {
    toast(`Could not rename the world: ${errMessage(err)}`, 'error');
  }
}

async function deleteWorld(inst: EnrichedInstance, worldName: string, onDone: () => void): Promise<void> {
  const choice = await showChoice<'delete'>(
    `Delete "${worldName}" from this instance. This removes the save folder from disk.`,
    [{ value: 'delete', label: 'Delete world', variant: 'danger' }],
    { title: 'Delete world' },
  );
  if (choice !== 'delete') return;
  try {
    const res: any = await api('DELETE', `/instances/${encodeURIComponent(inst.id)}/worlds/${encodeURIComponent(worldName)}`);
    if (res?.error) throw new Error(res.error);
    toast('World deleted');
    onDone();
  } catch (err) {
    toast(`Could not delete the world: ${errMessage(err)}`, 'error');
  }
}

async function backupWorld(inst: EnrichedInstance, worldName: string, onDone: () => void): Promise<void> {
  try {
    const res: any = await api('POST', `/instances/${encodeURIComponent(inst.id)}/worlds/${encodeURIComponent(worldName)}/backup`, {});
    if (res?.error) throw new Error(res.error);
    toast(res?.location ? `World backed up to ${res.location}` : 'World backed up');
    onDone();
  } catch (err) {
    toast(`Could not back up the world: ${errMessage(err)}`, 'error');
  }
}

function screenshotNameError(value: string, currentName?: string): string | null {
  const name = value.trim();
  if (!name || name === '.' || name === '..') return 'Use a screenshot filename.';
  if (name !== value) return 'Screenshot names cannot start or end with spaces.';
  if (name.startsWith('.')) return 'Screenshot names cannot start with a dot.';
  if (/[\\/]/.test(name)) return 'Screenshot names cannot include folders.';
  if (/[\u0000-\u001f\u007f]/.test(name)) return 'Screenshot names cannot include control characters.';
  if (!/\.(png|jpe?g|webp)$/i.test(name)) return 'Use a PNG, JPG, JPEG, or WEBP filename.';
  if (currentName && screenshotKind(name) !== screenshotKind(currentName)) return 'Keep the same screenshot file type.';
  return null;
}

function screenshotKind(name: string): 'png' | 'jpeg' | 'webp' | '' {
  const lower = name.toLowerCase();
  if (lower.endsWith('.png')) return 'png';
  if (lower.endsWith('.jpg') || lower.endsWith('.jpeg')) return 'jpeg';
  if (lower.endsWith('.webp')) return 'webp';
  return '';
}

function screenshotFileUrl(inst: EnrichedInstance, name: string): string {
  return apiResourceUrl(`/instances/${encodeURIComponent(inst.id)}/screenshots/${encodeURIComponent(name)}/file`);
}

async function renameScreenshot(inst: EnrichedInstance, screenshotName: string, onDone: () => void): Promise<void> {
  const next = await prompt('New name for this screenshot', screenshotName, {
    title: 'Rename screenshot',
    confirmText: 'Rename',
    validate: (value) => screenshotNameError(value, screenshotName),
  });
  const nextName = next ?? '';
  if (!nextName || nextName === screenshotName) return;
  try {
    const res: any = await api('PUT', `/instances/${encodeURIComponent(inst.id)}/screenshots/${encodeURIComponent(screenshotName)}`, { name: nextName });
    if (res?.error) throw new Error(res.error);
    toast('Screenshot renamed');
    onDone();
  } catch (err) {
    toast(`Could not rename the screenshot: ${errMessage(err)}`, 'error');
  }
}

async function deleteScreenshot(inst: EnrichedInstance, screenshotName: string, onDone: () => void): Promise<void> {
  const choice = await showChoice<'delete'>(
    `Delete "${screenshotName}" from this instance. This removes the screenshot file from disk.`,
    [{ value: 'delete', label: 'Delete screenshot', variant: 'danger' }],
    { title: 'Delete screenshot' },
  );
  if (choice !== 'delete') return;
  try {
    const res: any = await api('DELETE', `/instances/${encodeURIComponent(inst.id)}/screenshots/${encodeURIComponent(screenshotName)}`);
    if (res?.error) throw new Error(res.error);
    toast('Screenshot deleted');
    onDone();
  } catch (err) {
    toast(`Could not delete the screenshot: ${errMessage(err)}`, 'error');
  }
}

async function renameInstance(inst: EnrichedInstance): Promise<void> {
  const next = await prompt('New name for this instance', inst.name, { title: 'Rename instance', confirmText: 'Rename' });
  if (!next || next === inst.name) return;
  try {
    const res: any = await api('PUT', `/instances/${encodeURIComponent(inst.id)}`, { name: next });
    if (res.error) throw new Error(res.error);
    updateInstanceInList(res);
    toast('Renamed');
  } catch (err) {
    toast(`Could not rename the instance: ${errMessage(err)}`, 'error');
  }
}

async function duplicateInstance(inst: EnrichedInstance): Promise<void> {
  try {
    const res: any = await api('POST', `/instances/${encodeURIComponent(inst.id)}/duplicate`, {});
    if (res.error) throw new Error(res.error);
    addInstance(res);
    toast('Duplicated');
  } catch (err) {
    toast(`Could not duplicate the instance: ${errMessage(err)}`, 'error');
  }
}

async function deleteInstanceFlow(inst: EnrichedInstance, onDone?: () => void): Promise<void> {
  const choice = await showChoice<'keep-files' | 'delete-files'>(
    `Remove "${inst.name}" from the launcher but keep files on disk, or delete the instance and its saves, mods, and config.`,
    [
      { value: 'keep-files', label: 'Remove, keep files', variant: 'secondary' },
      { value: 'delete-files', label: 'Delete instance and files', variant: 'danger' },
    ],
    { title: 'Remove instance' },
  );
  if (!choice) return;
  const keepFiles = choice === 'keep-files';
  try {
    const suffix = keepFiles ? '?keep_files=true' : '';
    const res: any = await api('DELETE', `/instances/${encodeURIComponent(inst.id)}${suffix}`);
    if (res?.error) throw new Error(res.error);
    removeInstance(inst.id);
    toast(keepFiles ? 'Removed from launcher; files kept on disk' : 'Instance deleted');
    onDone?.();
  } catch (err) {
    toast(`Could not remove the instance: ${errMessage(err)}`, 'error');
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

function fmtBytes(bytes: number | undefined): string {
  const value = typeof bytes === 'number' && Number.isFinite(bytes) ? bytes : 0;
  if (value < 1024) return `${value} B`;
  const units = ['KB', 'MB', 'GB'];
  let next = value / 1024;
  let index = 0;
  while (next >= 1024 && index < units.length - 1) {
    next /= 1024;
    index += 1;
  }
  return `${next >= 10 ? next.toFixed(1) : next.toFixed(2)} ${units[index]}`;
}

type ResourceLoadState =
  | { status: 'loading'; data: InstanceResourceSummary | null; error?: undefined }
  | { status: 'ready'; data: InstanceResourceSummary; error?: undefined }
  | { status: 'error'; data: InstanceResourceSummary | null; error: string };

type PerformanceProgramState =
  | { status: 'loading'; plan: PerformancePlanResponse | null; health: PerformanceHealthResponse | null; error?: undefined }
  | { status: 'ready'; plan: PerformancePlanResponse | null; health: PerformanceHealthResponse | null; error?: undefined }
  | { status: 'error'; plan: PerformancePlanResponse | null; health: PerformanceHealthResponse | null; error: string };

function emptyResources(): InstanceResourceSummary {
  return {
    worlds: [],
    mods: [],
    screenshots: [],
    logs: [],
    worlds_count: 0,
    mods_count: 0,
    screenshots_count: 0,
    logs_count: 0,
  };
}

async function fetchInstanceResources(id: string): Promise<InstanceResourceSummary> {
  const res: any = await api('GET', `/instances/${encodeURIComponent(id)}/resources`);
  if (res?.error) throw new Error(res.error);
  return {
    ...emptyResources(),
    ...res,
    worlds: Array.isArray(res?.worlds) ? res.worlds : [],
    mods: Array.isArray(res?.mods) ? res.mods : [],
    screenshots: Array.isArray(res?.screenshots) ? res.screenshots : [],
    logs: Array.isArray(res?.logs) ? res.logs : [],
  };
}

type InstanceLogEntry = InstanceResourceSummary['logs'][number];
type LogSort = 'current' | 'newest' | 'name' | 'size';
type LogFilter = 'all' | 'important' | 'errors' | 'warnings' | 'system-info';
type LogLineKind = 'error' | 'warning' | 'system' | 'info';

interface ClassifiedLogLine {
  index: number;
  text: string;
  kind: LogLineKind;
  label: string;
  important: boolean;
}

const LOG_SORT_LABELS: Record<LogSort, string> = {
  current: 'Current/latest',
  newest: 'Newest',
  name: 'Name',
  size: 'Size',
};

const LOG_FILTER_LABELS: Record<LogFilter, string> = {
  all: 'All',
  important: 'Important',
  errors: 'Errors',
  warnings: 'Warnings',
  'system-info': 'System/info',
};

const LOG_TAIL_POLL_MS = 2500;
const LOG_RESOURCE_POLL_MS = 10000;

function currentLogRank(name: string): number {
  const lower = name.toLowerCase();
  if (lower === 'latest.log') return 0;
  if (lower === 'current.log') return 1;
  if (lower.includes('latest')) return 2;
  if (lower.includes('current')) return 3;
  return 10;
}

function isCurrentLog(name: string): boolean {
  return currentLogRank(name) < 10;
}

function sortLogs(logs: InstanceLogEntry[], sort: LogSort): InstanceLogEntry[] {
  const next = [...logs];
  next.sort((a, b) => {
    if (sort === 'name') return a.name.toLowerCase().localeCompare(b.name.toLowerCase());
    if (sort === 'size') return b.size - a.size || a.name.localeCompare(b.name);
    if (sort === 'current') {
      const current = currentLogRank(a.name) - currentLogRank(b.name);
      if (current !== 0) return current;
    }
    return b.modified_at.localeCompare(a.modified_at) || a.name.localeCompare(b.name);
  });
  return next;
}

function pickInitialLog(logs: InstanceLogEntry[]): string {
  return sortLogs(logs, 'current')[0]?.name ?? '';
}

function classifyLogLine(text: string): LogLineKind {
  const lower = text.toLowerCase();
  if (/\b(errors?|fatal|exceptions?|crashes?|crashed)\b/.test(lower)) return 'error';
  if (/\bwarn(?:ing|ings|ed)?\b/.test(lower)) return 'warning';
  if (/\b(launcher|system|guardian|healing|croopor)\b/.test(lower)) return 'system';
  return 'info';
}

function logLineLabel(kind: LogLineKind): string {
  if (kind === 'error') return 'ERR';
  if (kind === 'warning') return 'WARN';
  if (kind === 'system') return 'SYS';
  return 'INFO';
}

function classifyLogText(text: string): ClassifiedLogLine[] {
  if (!text) return [];
  const normalized = text.replace(/\r\n?/g, '\n');
  const rawLines = normalized.endsWith('\n') ? normalized.slice(0, -1).split('\n') : normalized.split('\n');
  return rawLines.map((line, index) => {
    const kind = classifyLogLine(line);
    return {
      index,
      text: line,
      kind,
      label: logLineLabel(kind),
      important: kind !== 'info',
    };
  });
}

function logLineMatchesFilter(line: ClassifiedLogLine, filter: LogFilter): boolean {
  if (filter === 'all') return true;
  if (filter === 'important') return line.important;
  if (filter === 'errors') return line.kind === 'error';
  if (filter === 'warnings') return line.kind === 'warning';
  return line.kind === 'system' || line.kind === 'info';
}

async function fetchLogTail(id: string, name: string): Promise<InstanceLogTail> {
  const res: InstanceLogTail & { error?: string } = await api('GET', `/instances/${encodeURIComponent(id)}/logs/${encodeURIComponent(name)}`);
  if (res?.error) throw new Error(res.error);
  return res;
}

function ResourceStatus({
  state,
  onRetry,
}: {
  state: ResourceLoadState;
  onRetry: () => void;
}): JSX.Element | null {
  if (state.status === 'loading' && !state.data) {
    return <div class="cp-resource-note">Loading files…</div>;
  }
  if (state.status === 'error') {
    return (
      <div class="cp-resource-note cp-resource-note--error">
        <span>{state.error}</span>
        <Button variant="secondary" size="sm" icon="refresh" onClick={onRetry}>Retry</Button>
      </div>
    );
  }
  return null;
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

function installTargetFor(inst: EnrichedInstance, version: Version | undefined): string {
  return version?.needs_install || version?.id || inst.version_id;
}

function performanceModeFrom(value: string | undefined): PerformanceMode | null {
  if (value === 'managed' || value === 'vanilla' || value === 'custom') return value;
  return null;
}

function globalPerformanceMode(): PerformanceMode {
  return performanceModeFrom(config.value?.performance_mode) ?? 'managed';
}

function effectivePerformanceMode(inst: EnrichedInstance): { mode: PerformanceMode; source: 'instance' | 'global' } {
  const instanceMode = performanceModeFrom(inst.performance_mode);
  if (instanceMode) return { mode: instanceMode, source: 'instance' };
  return { mode: globalPerformanceMode(), source: 'global' };
}

function performanceModeLabel(mode: PerformanceMode): string {
  if (mode === 'managed') return 'Managed';
  if (mode === 'vanilla') return 'Vanilla';
  return 'Custom';
}

function compositionTierLabel(tier: CompositionTier | ''): string {
  if (tier === 'extended') return 'Extended';
  if (tier === 'core') return 'Core';
  if (tier === 'vanilla_enhanced') return 'Vanilla enhanced';
  return 'Managed';
}

function healthLabel(health: PerformanceHealthStatus | undefined): string {
  if (health === 'healthy') return 'healthy';
  if (health === 'degraded') return 'degraded';
  if (health === 'fallback') return 'fallback';
  if (health === 'invalid') return 'needs attention';
  if (health === 'disabled') return 'not installed';
  return 'unknown';
}

function healthTone(health: PerformanceHealthStatus | undefined): 'ok' | 'warn' | 'err' | 'mute' {
  if (health === 'healthy') return 'ok';
  if (health === 'degraded' || health === 'fallback' || health === 'disabled') return 'warn';
  if (health === 'invalid') return 'err';
  return 'mute';
}

function planLoader(v: Version | undefined, inst: EnrichedInstance): string {
  const componentId = v?.loader?.component_id ?? '';
  if (componentId.includes('neoforged')) return 'neoforge';
  if (componentId.includes('minecraftforge')) return 'forge';
  if (componentId.includes('fabric')) return 'fabric';
  if (componentId.includes('quilt')) return 'quilt';
  const raw = inst.version_id.toLowerCase();
  if (raw.includes('neoforge')) return 'neoforge';
  if (raw.includes('fabric')) return 'fabric';
  if (raw.includes('forge')) return 'forge';
  if (raw.includes('quilt')) return 'quilt';
  return 'vanilla';
}

function planGameVersion(v: Version | undefined, inst: EnrichedInstance): string {
  return v?.minecraft_meta.effective_version
    || v?.minecraft_meta.base_id
    || v?.minecraft_meta.display_name
    || inst.version_id;
}

function performanceSummary(
  state: PerformanceProgramState,
  mode: PerformanceMode,
): { tone: 'ok' | 'warn' | 'err' | 'mute'; title: string; detail: string } {
  if (state.status === 'loading' && !state.plan && !state.health) {
    return {
      tone: 'mute',
      title: 'Checking plan',
      detail: 'Memory and Java evidence stays visible while Croopor reads bundle state.',
    };
  }
  if (state.status === 'error' && !state.plan && !state.health) {
    return {
      tone: 'mute',
      title: 'Plan status unavailable',
      detail: 'Backend plan data is not available right now.',
    };
  }
  if (mode === 'vanilla') {
    return {
      tone: 'mute',
      title: 'No managed bundle',
      detail: 'Memory allocation and Java detection are shown below.',
    };
  }
  if (mode === 'custom') {
    return {
      tone: 'mute',
      title: 'No managed bundle',
      detail: 'Memory allocation and Java detection are shown below.',
    };
  }

  const plan = state.plan;
  const health = state.health;
  if (!plan) {
    return {
      tone: 'mute',
      title: 'Bundle status unavailable',
      detail: 'Plan details are unavailable.',
    };
  }

  const tier = compositionTierLabel(plan.tier);
  const modCount = plan.mods?.length ?? 0;
  const composition = plan.composition_id ? `Composition ${plan.composition_id}` : 'No managed composition selected';
  const healthText = health ? `bundle ${healthLabel(health.health)}` : 'health not checked';
  const warning = health?.warnings?.[0] || plan.warnings?.[0] || plan.fallback_reason || '';

  if (health?.health === 'fallback') {
    const fallbackTier = health.tier ? compositionTierLabel(health.tier) : 'Managed';
    return {
      tone: healthTone(health.health),
      title: `${fallbackTier} fallback`,
      detail: warning || `Croopor safely lowered the requested ${tier} plan.`,
    };
  }

  return {
    tone: healthTone(health?.health),
    title: `${tier} plan`,
    detail: warning || `${composition}, ${modCount} managed mod${modCount === 1 ? '' : 's'}, ${healthText}.`,
  };
}

function performanceSummaryIcon(tone: 'ok' | 'warn' | 'err' | 'mute'): string {
  if (tone === 'ok') return 'check-circle';
  if (tone === 'warn' || tone === 'err') return 'alert';
  return 'info';
}

interface PerformanceInstallProgress {
  phase?: string;
  current?: number;
  total?: number;
  file?: string;
  error?: string;
  done?: boolean;
}

function performanceProgressTitle(progress: PerformanceInstallProgress): string {
  if (progress.phase === 'queued') return 'Bundle queued';
  if (progress.phase === 'planning') return 'Planning bundle';
  if (progress.phase === 'applying') return 'Applying bundle';
  if (progress.phase === 'removing') return 'Removing bundle';
  if (progress.phase === 'rolling_back') return 'Rolling back bundle';
  if (progress.phase === 'complete') return 'Bundle updated';
  if (progress.phase === 'error') return 'Bundle update failed';
  return 'Updating bundle';
}

function performanceProgressDetail(progress: PerformanceInstallProgress): string {
  if (progress.error) return progress.error;
  if (progress.file?.trim()) return progress.file;
  if (progress.phase === 'queued') return 'Waiting to update managed performance files.';
  if (progress.phase === 'planning') return 'Checking the managed performance plan.';
  if (progress.phase === 'applying') return 'Applying managed performance files.';
  if (progress.phase === 'removing') return 'Removing managed performance files.';
  if (progress.phase === 'rolling_back') return 'Rolling back managed performance files.';
  if (progress.phase === 'complete') return 'Managed performance update complete.';
  if (progress.phase === 'error') return 'Performance update failed.';
  return 'Updating managed performance files.';
}

function isPerformanceOperationTerminal(status: PerformanceOperationStatus): boolean {
  return status.state === 'complete' || status.state === 'failed' || status.state === 'interrupted';
}

function isPerformanceOperationComplete(status: PerformanceOperationStatus): boolean {
  return status.state === 'complete';
}

function operationStatusAsProgress(status: PerformanceOperationStatus): PerformanceInstallProgress {
  const failed = status.state === 'failed' || status.state === 'interrupted';
  const phase = failed ? 'error' : status.state;
  const current = phase === 'queued'
    ? 0
    : phase === 'planning'
      ? 1
      : phase === 'complete' || phase === 'error'
        ? 4
        : 2;
  return {
    phase,
    current,
    total: 4,
    error: failed ? status.error || 'performance operation failed' : status.error,
    done: isPerformanceOperationTerminal(status),
  };
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

function WorldsCard({
  inst,
  resources,
  onOpenWorlds,
}: {
  inst: EnrichedInstance;
  resources: InstanceResourceSummary | null;
  onOpenWorlds: () => void;
}): JSX.Element {
  const worlds = resources?.worlds ?? [];
  const count = resources
    ? Math.max(resources.worlds_count ?? 0, worlds.length)
    : inst.saves_count ?? 0;
  const visibleWorlds = worlds.slice(0, 3);
  const hiddenWorlds = visibleWorlds.length > 0 ? Math.max(count - visibleWorlds.length, 0) : 0;
  const footerCopy = visibleWorlds.length === 0
    ? 'Open the Worlds tab to see saves'
    : hiddenWorlds > 0
      ? `${hiddenWorlds} more world${hiddenWorlds === 1 ? '' : 's'} in Worlds`
      : `${count} world${count === 1 ? '' : 's'} available`;
  return (
    <Card padding={18} class={`cp-od-worlds-card${count === 0 ? ' cp-od-worlds-card--empty' : ''}`}>
      <div class="cp-od-head">
        <h3>Worlds{count > 0 ? <span class="cp-od-head-count">· {count}</span> : null}</h3>
        <button class="cp-od-overflow" type="button" aria-label="More" onClick={(e) => openContextMenu(e, [
          { icon: 'folder', label: 'Open saves folder', onSelect: () => void openInstanceFolder(inst.id, 'saves') },
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
            <Button icon="globe" onClick={onOpenWorlds} sound="affirm">View worlds</Button>
            <Button variant="ghost" icon="folder" onClick={() => void openInstanceFolder(inst.id, 'saves')}>Import world</Button>
          </div>
        </div>
      ) : (
        <div class="cp-od-worlds-list">
          {visibleWorlds.length > 0 ? visibleWorlds.map((world) => (
            <div class="cp-od-world-row" key={world.name}>
              <div class="cp-od-world-mark"><Icon name="globe" size={16} /></div>
              <div class="cp-od-world-body">
                <div class="cp-od-world-name" title={world.name}>{world.name}</div>
                <div class="cp-od-world-sub">{fmtBytes(world.size)} · changed {fmtRelative(world.modified_at)}</div>
              </div>
            </div>
          )) : (
            <div class="cp-od-world-row">
              <div class="cp-od-world-mark"><Icon name="globe" size={16} /></div>
              <div class="cp-od-world-body">
                <div class="cp-od-world-name">{count} save{count === 1 ? '' : 's'} on disk</div>
                <div class="cp-od-world-sub">Last touched {fmtRelative(inst.last_played_at)}</div>
              </div>
            </div>
          )}
          <div class="cp-od-worlds-footer">
            <span>{footerCopy}</span>
            <button class="cp-od-link" type="button" onClick={onOpenWorlds}>
              View worlds <Icon name="chevron-right" size={11} stroke={2.2} />
            </button>
          </div>
        </div>
      )}
    </Card>
  );
}

// ─── Activity — replaces "Recent events"; small, human-readable ──────────

interface ActivityItem { label: string; relative: string }

function ActivityCard({
  inst,
  resources,
  onOpenLogs,
}: {
  inst: EnrichedInstance;
  resources: InstanceResourceSummary | null;
  onOpenLogs: () => void;
}): JSX.Element {
  const events: ActivityItem[] = useMemo(() => {
    const out: ActivityItem[] = [];
    out.push({ label: 'Instance created', relative: fmtRelative(inst.created_at) });
    if (inst.last_played_at) {
      out.unshift({ label: 'Last launch session', relative: fmtRelative(inst.last_played_at) });
    }
    const latestLog = resources?.logs[0];
    if (latestLog) {
      out.push({ label: `Latest log: ${latestLog.name}`, relative: fmtRelative(latestLog.modified_at) });
    }
    const latestWorld = resources?.worlds[0];
    if (latestWorld) {
      out.push({ label: `World changed: ${latestWorld.name}`, relative: fmtRelative(latestWorld.modified_at) });
    }
    return out.slice(0, 3);
  }, [inst.id, inst.created_at, inst.last_played_at, resources]);

  return (
    <Card padding={18}>
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

function LogsCard({
  inst,
  resources,
  running,
  onOpenLogs,
}: {
  inst: EnrichedInstance;
  resources: InstanceResourceSummary | null;
  running: boolean;
  onOpenLogs: () => void;
}): JSX.Element {
  const latest = pickInitialLog(resources?.logs ?? []);
  const latestLog = latest ? resources?.logs.find((log) => log.name === latest) : undefined;
  const count = resources?.logs_count ?? 0;
  const [tail, setTail] = useState<{ status: 'idle' | 'loading' | 'ready' | 'error'; data?: InstanceLogTail; error?: string }>({ status: 'idle' });
  const importantLines = useMemo(() => {
    if (tail.status !== 'ready') return [];
    return classifyLogText(tail.data?.text ?? '').filter((line) => line.important).slice(-3);
  }, [tail.data?.text, tail.status]);
  const summary = latestLog ? `${latestLog.name} · ${fmtRelative(latestLog.modified_at)}` : 'No launch logs on disk yet';

  useEffect(() => {
    if (!latest) {
      setTail({ status: 'idle' });
      return;
    }
    let alive = true;
    const load = (showLoading: boolean): void => {
      if (showLoading) {
        setTail((current) => current.data?.name === latest ? current : { status: 'loading' });
      }
      void fetchLogTail(inst.id, latest)
        .then((data) => {
          if (alive) setTail({ status: 'ready', data });
        })
        .catch((err) => {
          if (alive) setTail({ status: 'error', error: errMessage(err) });
        });
    };
    load(true);
    const timer = running ? window.setInterval(() => load(false), LOG_TAIL_POLL_MS) : 0;
    return () => {
      alive = false;
      if (timer) window.clearInterval(timer);
    };
  }, [inst.id, latest, running]);

  return (
    <Card padding={16} class="cp-od-logs-card">
      <div class="cp-od-logs-summary">
        <span class="cp-od-logs-icon"><Icon name="terminal" size={14} stroke={1.9} /></span>
        <div class="cp-od-logs-line">
          <strong>Logs</strong>
          <span class="cp-od-logs-sub">{summary}</span>
        </div>
        <button class="cp-od-link" type="button" onClick={onOpenLogs}>
          {count > 0 ? `View ${count}` : 'View logs'} <Icon name="chevron-right" size={11} stroke={2.2} />
        </button>
      </div>
      {importantLines.length > 0 ? (
        <div class="cp-od-log-tail" aria-label="Important latest log lines">
          {importantLines.map((line) => (
            <LogLine line={line} compact key={line.index} />
          ))}
        </div>
      ) : tail.status === 'error' ? (
        <div class="cp-od-log-note">Could not load latest log.</div>
      ) : null}
    </Card>
  );
}

function QuickActionsCard({
  inst,
  running,
  onLaunch,
  onStop,
  onOpenLogs,
}: {
  inst: EnrichedInstance;
  running: boolean;
  onLaunch: () => void;
  onStop: () => void;
  onOpenLogs: () => void;
}): JSX.Element {
  return (
    <Card padding={18} class="cp-od-quick-card">
      <div class="cp-od-head">
        <h3>Quick actions</h3>
      </div>
      <div class="cp-od-quick-grid">
        <button
          class="cp-od-quick-action"
          type="button"
          onClick={() => void openInstanceFolder(inst.id, 'resourcepacks')}
        >
          <span class="cp-od-quick-icon"><Icon name="image" size={15} stroke={1.9} /></span>
          <span class="cp-od-quick-copy">
            <strong>Resource packs</strong>
            <span>Open resource packs</span>
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

// ─── Performance — overview health and runtime evidence. Settings owns policy controls.

function memoryGb(valueMb: number | undefined, fallbackMb: number): number {
  const mb = typeof valueMb === 'number' && valueMb > 0 ? valueMb : fallbackMb;
  return Math.max(0.5, mb / 1024);
}

function PerformanceCard({ inst }: { inst: EnrichedInstance }): JSX.Element {
  const version = versions.value.find(v => v.id === inst.version_id);
  const effectiveMode = effectivePerformanceMode(inst);
  const maxMem = memoryGb(inst.max_memory_mb, config.value?.max_memory_mb ?? 4096);
  const minMem = memoryGb(inst.min_memory_mb, config.value?.min_memory_mb ?? 1024);
  const modeSourceLabel = effectiveMode.source === 'instance' ? 'instance override' : 'global default';
  const [program, setProgram] = useState<PerformanceProgramState>({ status: 'loading', plan: null, health: null });
  const [lifecycleOperation, setLifecycleOperation] = useState<PerformanceOperationStatus | null>(null);
  const operationPollRef = useRef<number | null>(null);
  const operationRequestRef = useRef(0);

  const fetchPerformanceProgram = useCallback(async (): Promise<{
    plan: PerformancePlanResponse | null;
    health: PerformanceHealthResponse | null;
  }> => {
    const gameVersion = planGameVersion(version, inst);
    const loader = planLoader(version, inst);
    const planParams = new URLSearchParams({
      game_version: gameVersion,
      loader,
      mode: effectiveMode.mode,
      instance_id: inst.id,
    });
    const healthParams = new URLSearchParams({ instance_id: inst.id });
    const [planRes, healthRes]: [any, any] = await Promise.all([
      api('GET', `/performance/plan?${planParams.toString()}`),
      api('GET', `/performance/health?${healthParams.toString()}`),
    ]);
    if (planRes?.error) throw new Error(planRes.error);
    if (healthRes?.error) throw new Error(healthRes.error);
    return {
      plan: planRes?.mode ? planRes as PerformancePlanResponse : null,
      health: healthRes?.health ? healthRes as PerformanceHealthResponse : null,
    };
  }, [inst.id, inst.version_id, version?.id, version?.loader?.component_id, version?.minecraft_meta.effective_version, effectiveMode.mode]);

  useEffect(() => {
    return () => {
      if (operationPollRef.current !== null) window.clearInterval(operationPollRef.current);
    };
  }, []);

  useEffect(() => {
    let alive = true;
    setProgram(current => ({ status: 'loading', plan: current.plan, health: current.health }));
    void fetchPerformanceProgram()
      .then(({ plan, health }) => {
        if (!alive) return;
        setProgram({
          status: 'ready',
          plan,
          health,
        });
      })
      .catch((err) => {
        if (!alive) return;
        setProgram(current => ({
          status: 'error',
          plan: current.plan,
          health: current.health,
          error: errMessage(err),
        }));
      });

    return () => { alive = false; };
  }, [fetchPerformanceProgram]);

  useEffect(() => {
    let alive = true;
    const requestId = operationRequestRef.current + 1;
    operationRequestRef.current = requestId;
    if (operationPollRef.current !== null) {
      window.clearInterval(operationPollRef.current);
      operationPollRef.current = null;
    }

    const applyStatus = (status: PerformanceOperationStatus | null): boolean => {
      if (!alive || requestId !== operationRequestRef.current) return true;
      if (status && isPerformanceOperationComplete(status)) {
        setLifecycleOperation(null);
        return true;
      }
      setLifecycleOperation(status);
      return !status || isPerformanceOperationTerminal(status);
    };

    const refreshAfterComplete = async (): Promise<void> => {
      const refreshed = await fetchPerformanceProgram();
      if (alive && requestId === operationRequestRef.current) {
        setProgram({ status: 'ready', ...refreshed });
      }
    };

    const pollStatus = async (operationId: string): Promise<void> => {
      try {
        const res: any = await api(
          'GET',
          `/performance/operations/${encodeURIComponent(operationId)}`,
        );
        if (!res?.id && res?.error) throw new Error(res.error);
        const status = res as PerformanceOperationStatus;
        const terminal = applyStatus(status);
        if (terminal && operationPollRef.current !== null) {
          window.clearInterval(operationPollRef.current);
          operationPollRef.current = null;
        }
        if (terminal && isPerformanceOperationComplete(status)) {
          await refreshAfterComplete();
        }
      } catch {
        if (alive && requestId === operationRequestRef.current) {
          applyStatus(null);
          if (operationPollRef.current !== null) {
            window.clearInterval(operationPollRef.current);
            operationPollRef.current = null;
          }
        }
      }
    };

    void (async () => {
      try {
        const res: PerformanceInstanceOperationResponse & { error?: string } = await api(
          'GET',
          `/performance/instances/${encodeURIComponent(inst.id)}/operation`,
        );
        if (res?.error) throw new Error(res.error);
        const operation = res.operation ?? null;
        const terminal = applyStatus(operation);
        if (operation && isPerformanceOperationComplete(operation)) {
          await refreshAfterComplete();
          return;
        }
        if (operation && !terminal) {
          operationPollRef.current = window.setInterval(() => {
            void pollStatus(operation.id);
          }, 1250);
          void pollStatus(operation.id);
        }
      } catch {
        applyStatus(null);
      }
    })();

    return () => {
      alive = false;
      if (operationPollRef.current !== null) {
        window.clearInterval(operationPollRef.current);
        operationPollRef.current = null;
      }
    };
  }, [inst.id, fetchPerformanceProgram]);

  const baseSummary = performanceSummary(program, effectiveMode.mode);
  const operationProgress = lifecycleOperation ? operationStatusAsProgress(lifecycleOperation) : null;
  const visibleLifecycleProgress = operationProgress
    ? {
      title: performanceProgressTitle(operationProgress),
      detail: performanceProgressDetail(operationProgress),
    }
    : null;
  const summary = visibleLifecycleProgress
    ? {
      tone: operationProgress?.phase === 'error'
        ? 'err' as const
        : operationProgress?.done
          ? 'ok' as const
          : 'mute' as const,
      title: visibleLifecycleProgress.title || 'Updating bundle',
      detail: visibleLifecycleProgress.detail || 'Croopor is updating managed performance files.',
    }
    : baseSummary;
  const summaryIcon = performanceSummaryIcon(summary.tone);
  const planTier = program.plan ? compositionTierLabel(program.plan.tier) : performanceModeLabel(effectiveMode.mode);
  const managedCount = program.plan?.mods?.length ?? program.health?.installed_count ?? 0;
  const runtimeLabel = inst.java_major ? `Java ${inst.java_major} detected` : 'Managed Java detection';

  return (
    <Card padding={18}>
      <div class="cp-od-head">
        <h3>Performance</h3>
      </div>

      <div class="cp-od-perf-summary" data-tone={summary.tone} aria-live="polite">
        <span class="cp-od-perf-summary-mark">
          <Icon name={summaryIcon} size={16} stroke={2.4} />
        </span>
        <div class="cp-od-perf-summary-copy">
          <strong>{summary.title}</strong>
          <span>{summary.detail}</span>
        </div>
      </div>

      <div class="cp-od-perf-facts">
        <div class="cp-od-perf-row">
          <span class="cp-od-perf-key">Mode</span>
          <span class="cp-od-perf-val">{performanceModeLabel(effectiveMode.mode)} ({modeSourceLabel})</span>
        </div>
        <div class="cp-od-perf-row">
          <span class="cp-od-perf-key">Memory</span>
          <span class="cp-od-perf-val">{fmtMem(minMem)} to {fmtMem(maxMem)}</span>
        </div>
        <div class="cp-od-perf-row">
          <span class="cp-od-perf-key">Plan evidence</span>
          <span class="cp-od-perf-val">{planTier}, {managedCount} managed mod{managedCount === 1 ? '' : 's'}</span>
        </div>
        <div class="cp-od-perf-runtime">
          <span class="cp-od-perf-runtime-mark"><Icon name="check" size={12} stroke={2.6} /></span>
          <span class="cp-od-perf-runtime-text">{runtimeLabel}</span>
        </div>
      </div>
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
    <Card padding={18}>
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

function OverviewPane({ inst, resources, running, onLaunch, onStop, onOpenWorlds, onOpenLogs }: {
  inst: EnrichedInstance;
  resources: InstanceResourceSummary | null;
  running: boolean;
  onLaunch: () => void;
  onStop: () => void;
  onOpenWorlds: () => void;
  onOpenLogs: () => void;
}): JSX.Element {
  return (
    <div class="cp-instance-body cp-instance-body--overview-bento">
      <div class="cp-od-stagger cp-od-slot cp-od-slot--performance" style={{ '--cp-od-delay': '0ms' } as any}>
        <PerformanceCard inst={inst} />
      </div>
      <div class="cp-od-stagger cp-od-slot cp-od-slot--worlds cp-od-worlds-slot" style={{ '--cp-od-delay': '80ms' } as any}>
        <WorldsCard inst={inst} resources={resources} onOpenWorlds={onOpenWorlds} />
      </div>
      <div class="cp-od-stagger cp-od-slot cp-od-slot--activity" style={{ '--cp-od-delay': '120ms' } as any}>
        <ActivityCard inst={inst} resources={resources} onOpenLogs={onOpenLogs} />
      </div>
      <div class="cp-od-stagger cp-od-slot cp-od-slot--quick" style={{ '--cp-od-delay': '160ms' } as any}>
        <QuickActionsCard
          inst={inst}
          running={running}
          onLaunch={onLaunch}
          onStop={onStop}
          onOpenLogs={onOpenLogs}
        />
      </div>
      <div class="cp-od-stagger cp-od-slot cp-od-slot--details" style={{ '--cp-od-delay': '200ms' } as any}>
        <DetailsCard inst={inst} running={running} />
      </div>
    </div>
  );
}

function launchNoticeIcon(tone: LaunchNoticeTone): string {
  if (tone === 'success') return 'check-circle';
  if (tone === 'error') return 'alert';
  if (tone === 'warned') return 'alert';
  if (tone === 'intervened') return 'shield-check';
  return 'info';
}

function LaunchOutcomeNotice({ inst, notice }: {
  inst: EnrichedInstance;
  notice: LaunchNotice;
}): JSX.Element {
  const details = (notice.details ?? []).map(detail => detail.trim()).filter(Boolean);
  const primaryDetail = notice.detail?.trim() || (details.length === 1 ? details[0] : '');
  const listDetails = details.length > 1
    ? details.filter(detail => !primaryDetail || detail !== primaryDetail)
    : [];

  return (
    <div class="cp-instance-notice-shell">
      <section class="cp-launch-notice" data-tone={notice.tone} aria-live="polite">
        <span class="cp-launch-notice-mark" aria-hidden="true">
          <Icon name={launchNoticeIcon(notice.tone)} size={15} stroke={2.2} />
        </span>
        <div class="cp-launch-notice-copy">
          <strong>{notice.message}</strong>
          {primaryDetail && <p>{primaryDetail}</p>}
          {listDetails.length > 0 && (
            <details class="cp-launch-notice-details">
              <summary>Details</summary>
              <ul>
                {listDetails.map((detail, index) => <li key={`${index}:${detail}`}>{detail}</li>)}
              </ul>
            </details>
          )}
        </div>
        <button
          class="cp-launch-notice-dismiss"
          type="button"
          aria-label="Dismiss launch notice"
          onClick={() => clearLaunchNotice(inst.id)}
        >
          <Icon name="x" size={13} stroke={2.2} />
        </button>
      </section>
    </div>
  );
}

function LaunchSplitButton({
  inst,
  canLaunch,
  installQueued,
  installProgress,
  onLaunch,
  onInstall,
  onOpenLogs,
  onOpenSettings,
  preparing,
}: {
  inst: EnrichedInstance;
  canLaunch: boolean;
  installQueued: boolean;
  installProgress: { pct: number; label: string } | null;
  onLaunch: () => void;
  onInstall: () => void;
  onOpenLogs: () => void;
  onOpenSettings: () => void;
  preparing: Extract<LaunchState, { status: 'preparing' }> | null;
}): JSX.Element {
  const progress = preparing
    ? { pct: preparing.pct, label: preparing.label }
    : installProgress;
  const needsInstall = !canLaunch;
  const label = progress?.label || (installQueued ? 'Queued' : needsInstall ? 'Install' : 'Launch');
  const icon = progress || installQueued ? 'clock' : needsInstall ? 'download' : 'play';
  const pct = progress?.pct ?? 0;
  const disabled = Boolean(progress) || installQueued;
  const primaryAction = needsInstall ? onInstall : onLaunch;
  const primaryMenuItem = needsInstall
    ? {
        icon: installQueued ? 'clock' : 'download',
        label: installQueued ? 'Queued' : 'Install',
        onSelect: installQueued ? () => toast('Install already queued') : onInstall,
      }
    : { icon: 'play', label: 'Launch now', onSelect: onLaunch };
  return (
    <div
      class={`cp-instance-split-launch${progress ? ' cp-instance-split-launch--preparing' : ''}`}
      role="group"
      aria-label="Instance actions"
      style={{ '--cp-launch-pct': `${pct}%` } as any}
    >
      {progress && <span class="cp-instance-split-launch-fill" aria-hidden="true" />}
      <button
        class="cp-instance-split-launch-main"
        type="button"
        onClick={disabled ? undefined : primaryAction}
        data-sound={needsInstall ? 'bright' : 'launchPress'}
        disabled={disabled}
      >
        <Icon name={icon} size={18} stroke={1.8} />
        <span>{label}</span>
      </button>
      <button
        class="cp-instance-split-launch-menu"
        type="button"
        aria-label="Instance options"
        aria-haspopup="menu"
        disabled={Boolean(progress)}
        onClick={(e) => openContextMenu(e, [
          primaryMenuItem,
          { icon: 'settings', label: 'Launch settings', onSelect: onOpenSettings },
          { icon: 'terminal', label: 'View launch logs', onSelect: onOpenLogs },
          { label: '', onSelect: () => {}, divider: true },
          { icon: 'folder', label: 'Open instance folder', onSelect: () => void openInstanceFolder(inst.id) },
          { icon: 'folder', label: 'Open resource packs folder', onSelect: () => void openInstanceFolder(inst.id, 'resourcepacks') },
          { icon: 'folder', label: 'Open shader packs folder', onSelect: () => void openInstanceFolder(inst.id, 'shaderpacks') },
        ])}
      >
        <Icon name="chevron-down" size={16} stroke={2.3} />
      </button>
      {progress && <span class="cp-instance-launch-status">{Math.round(pct)}%</span>}
    </div>
  );
}

function InstallBarrierPane({
  installTarget,
  installQueued,
  installProgress,
}: {
  installTarget: string;
  installQueued: boolean;
  installProgress: { pct: number; label: string } | null;
}): JSX.Element {
  const pct = installProgress ? Math.max(0, Math.min(100, Math.round(installProgress.pct))) : 0;
  const label = installProgress?.label || (installQueued ? 'Waiting for the current download slot' : 'Preparing install');
  const detail = installProgress
    ? `${pct}% complete`
    : installQueued
      ? 'This instance will unlock automatically after its version install starts and finishes.'
      : 'Croopor is preparing the required version files.';

  return (
    <div class="cp-instance-install-lock" aria-live="polite">
      <div class="cp-instance-install-lock-main">
        <span class="cp-instance-install-lock-icon" aria-hidden="true">
          <Icon name={installQueued ? 'clock' : 'download'} size={18} stroke={2} />
        </span>
        <div class="cp-instance-install-lock-copy">
          <h2>{installQueued ? 'Install queued' : 'Installing required files'}</h2>
          <p>{label} for {installTarget}.</p>
        </div>
      </div>

      <div class="cp-instance-install-lock-progress" style={{ '--cp-install-lock-pct': `${pct}%` } as any}>
        <span aria-hidden="true" />
      </div>

      <div class="cp-instance-install-lock-foot">
        <span>{detail}</span>
        <Button variant="secondary" size="sm" icon="download" onClick={() => navigate({ name: 'downloads' })}>
          Downloads
        </Button>
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

type ModFilter = 'all' | 'enabled' | 'disabled';

function ModsPane({
  inst,
  resources,
  onRefresh,
}: {
  inst: EnrichedInstance;
  resources: ResourceLoadState;
  onRefresh: () => void;
}): JSX.Element {
  const [q, setQ] = useState('');
  const [filter, setFilter] = useState<ModFilter>('all');
  const mods = resources.data?.mods ?? [];
  const filteredMods = mods.filter((mod) => {
    const matchesSearch = mod.name.toLowerCase().includes(q.trim().toLowerCase());
    const matchesFilter = filter === 'all' || (filter === 'enabled' ? mod.enabled : !mod.enabled);
    return matchesSearch && matchesFilter;
  });

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
          {(['all', 'enabled', 'disabled'] as ModFilter[]).map(f => (
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
        <Button variant="secondary" size="sm" icon="refresh" onClick={onRefresh}>Refresh</Button>
        <Button
          variant="soft"
          size="sm"
          icon="plus"
          onClick={() => void openInstanceFolder(inst.id, 'mods')}
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
        <ResourceStatus state={resources} onRetry={onRefresh} />
        {resources.status !== 'loading' && filteredMods.length === 0 ? (
          <div class="cp-mods-empty-row">
            <strong>{mods.length === 0 ? 'No mods installed in this instance' : 'No mods match this filter'}</strong>
            Drop jar files into the mods folder. In-app mod browsing and metadata are still backend-team work.
          </div>
        ) : (
          filteredMods.map((mod) => (
            <div class="cp-mods-table-row" data-disabled={!mod.enabled} key={mod.name}>
              <span><Icon name="puzzle" size={15} color="var(--text-dim)" /></span>
              <span class="cp-mods-file-icon">JAR</span>
              <span class="cp-resource-name" title={mod.name}>{mod.name}</span>
              <span>Local</span>
              <span>{fmtBytes(mod.size)}</span>
              <span>{mod.enabled ? 'Enabled' : 'Disabled'}</span>
              <span />
            </div>
          ))
        )}
      </div>
    </div>
  );
}

function WorldsPane({
  inst,
  resources,
  onRefresh,
}: {
  inst: EnrichedInstance;
  resources: ResourceLoadState;
  onRefresh: () => void;
}): JSX.Element {
  const worlds = resources.data?.worlds ?? [];
  return (
    <div class="cp-instance-body" style={{ display: 'block' }}>
      <ResourceToolbar
        title={`${worlds.length} world${worlds.length === 1 ? '' : 's'}`}
        onRefresh={onRefresh}
        action={{ icon: 'folder', label: 'Open saves', onClick: () => void openInstanceFolder(inst.id, 'saves') }}
      />
      <ResourceStatus state={resources} onRetry={onRefresh} />
      {worlds.length === 0 && resources.status !== 'loading' ? (
        <ResourceEmpty icon="globe" title="No saves yet" hint="Create a world in Minecraft or place an existing save in this instance's saves folder." />
      ) : (
        <div class="cp-resource-list">
          {worlds.map((world) => (
            <ResourceRow
              key={world.name}
              icon="globe"
              name={world.name}
              meta={`${fmtBytes(world.size)} · changed ${fmtRelative(world.modified_at)}`}
              actions={(
                <button
                  class="cp-resource-action"
                  type="button"
                  aria-label={`World actions for ${world.name}`}
                  onClick={(e) => openContextMenu(e, [
                    { icon: 'edit', label: 'Rename', onSelect: () => void renameWorld(inst, world.name, onRefresh) },
                    { icon: 'archive', label: 'Back up', onSelect: () => void backupWorld(inst, world.name, onRefresh) },
                    { divider: true, label: '', onSelect: () => undefined },
                    { icon: 'trash', label: 'Delete', onSelect: () => void deleteWorld(inst, world.name, onRefresh), danger: true },
                  ])}
                >
                  <Icon name="dots" size={15} />
                </button>
              )}
            />
          ))}
        </div>
      )}
    </div>
  );
}

type ScreenshotSort = 'newest' | 'name' | 'size';

const SCREENSHOT_SORT_LABELS: Record<ScreenshotSort, string> = {
  newest: 'Newest',
  name: 'Name',
  size: 'Size',
};

function ScreenshotsPane({
  inst,
  resources,
  onRefresh,
}: {
  inst: EnrichedInstance;
  resources: ResourceLoadState;
  onRefresh: () => void;
}): JSX.Element {
  const screenshots = resources.data?.screenshots ?? [];
  const [sort, setSort] = useState<ScreenshotSort>('newest');
  const [viewer, setViewer] = useState<string>('');
  const sortedScreenshots = useMemo(() => {
    const next = [...screenshots];
    next.sort((a, b) => {
      if (sort === 'name') return a.name.toLowerCase().localeCompare(b.name.toLowerCase());
      if (sort === 'size') return b.size - a.size || a.name.localeCompare(b.name);
      return b.modified_at.localeCompare(a.modified_at) || a.name.localeCompare(b.name);
    });
    return next;
  }, [screenshots, sort]);
  const viewedShot = viewer ? screenshots.find((shot) => shot.name === viewer) : undefined;

  useEffect(() => {
    if (viewer && !screenshots.some((shot) => shot.name === viewer)) setViewer('');
  }, [screenshots, viewer]);

  return (
    <div class="cp-instance-body" style={{ display: 'block' }}>
      <div class="cp-resource-toolbar cp-screenshots-toolbar">
        <strong>{screenshots.length} screenshot{screenshots.length === 1 ? '' : 's'}</strong>
        <div class="cp-screenshots-tools">
          <div class="cp-mini-seg" role="tablist" aria-label="Sort screenshots">
            {(Object.keys(SCREENSHOT_SORT_LABELS) as ScreenshotSort[]).map((item) => (
              <button
                key={item}
                type="button"
                role="tab"
                aria-selected={sort === item}
                data-active={sort === item}
                onClick={() => setSort(item)}
              >
                {SCREENSHOT_SORT_LABELS[item]}
              </button>
            ))}
          </div>
          <Button variant="secondary" size="sm" icon="refresh" onClick={onRefresh}>Refresh</Button>
          <Button variant="soft" size="sm" icon="folder" onClick={() => void openInstanceFolder(inst.id, 'screenshots')}>Open screenshots</Button>
        </div>
      </div>
      <ResourceStatus state={resources} onRetry={onRefresh} />
      {screenshots.length === 0 && resources.status !== 'loading' ? (
        <ResourceEmpty icon="image" title="No screenshots yet" hint="Minecraft saves screenshots here after you capture them in game." />
      ) : (
        <div class="cp-screenshots-grid">
          {sortedScreenshots.map((shot) => (
            <div class="cp-screenshot-tile" key={shot.name}>
              <button
                class="cp-screenshot-thumb"
                type="button"
                aria-label={`View ${shot.name}`}
                onClick={() => setViewer(shot.name)}
              >
                <img src={screenshotFileUrl(inst, shot.name)} alt="" loading="lazy" />
              </button>
              <div class="cp-screenshot-caption">
                <div class="cp-screenshot-text">
                  <div class="cp-screenshot-name" title={shot.name}>{shot.name}</div>
                  <div class="cp-screenshot-meta">{fmtBytes(shot.size)} · {fmtRelative(shot.modified_at)}</div>
                </div>
                <button
                  class="cp-resource-action"
                  type="button"
                  aria-label={`Screenshot actions for ${shot.name}`}
                  onClick={(e) => openContextMenu(e, [
                    { icon: 'image', label: 'View', onSelect: () => setViewer(shot.name) },
                    { icon: 'edit', label: 'Rename', onSelect: () => void renameScreenshot(inst, shot.name, onRefresh) },
                    { divider: true, label: '', onSelect: () => undefined },
                    { icon: 'trash', label: 'Delete', onSelect: () => void deleteScreenshot(inst, shot.name, onRefresh), danger: true },
                  ])}
                >
                  <Icon name="dots" size={15} />
                </button>
              </div>
            </div>
          ))}
        </div>
      )}
      {viewedShot ? (
        <div
          class="cp-screenshot-viewer"
          role="dialog"
          aria-modal="true"
          aria-label={viewedShot.name}
          onClick={() => setViewer('')}
          onKeyDown={(e: KeyboardEvent) => { if (e.key === 'Escape') setViewer(''); }}
        >
          <div class="cp-screenshot-viewer-panel" onClick={(e) => e.stopPropagation()}>
            <div class="cp-screenshot-viewer-bar">
              <div>
                <strong title={viewedShot.name}>{viewedShot.name}</strong>
                <span>{fmtBytes(viewedShot.size)} · {fmtRelative(viewedShot.modified_at)}</span>
              </div>
              <button class="cp-resource-action" type="button" aria-label="Close screenshot viewer" onClick={() => setViewer('')}>
                <Icon name="x" size={15} />
              </button>
            </div>
            <img src={screenshotFileUrl(inst, viewedShot.name)} alt={viewedShot.name} />
          </div>
        </div>
      ) : null}
    </div>
  );
}

function LogLine({ line, compact = false }: { line: ClassifiedLogLine; compact?: boolean }): JSX.Element {
  return (
    <div class={`cp-log-line${compact ? ' cp-log-line--compact' : ''}`} data-kind={line.kind}>
      <span class="cp-log-line-label" aria-label={`${line.kind} log line`}>{line.label}</span>
      <span class="cp-log-line-text">{line.text || ' '}</span>
    </div>
  );
}

function LogLines({ text, filter }: { text: string; filter: LogFilter }): JSX.Element {
  const lines = useMemo(() => classifyLogText(text), [text]);
  const filteredLines = useMemo(() => lines.filter((line) => logLineMatchesFilter(line, filter)), [filter, lines]);

  if (lines.length === 0) {
    return <div class="cp-log-empty">Log file is empty.</div>;
  }
  if (filteredLines.length === 0) {
    return <div class="cp-log-empty">No lines match this filter.</div>;
  }
  return (
    <div class="cp-log-lines" role="log" aria-label="Log preview">
      {filteredLines.map((line) => (
        <LogLine line={line} key={line.index} />
      ))}
    </div>
  );
}

function LogsPane({
  inst,
  resources,
  running,
  onRefresh,
}: {
  inst: EnrichedInstance;
  resources: ResourceLoadState;
  running: boolean;
  onRefresh: () => void;
}): JSX.Element {
  const logs = resources.data?.logs ?? [];
  const [selected, setSelected] = useState<string>('');
  const [sort, setSort] = useState<LogSort>('current');
  const [filter, setFilter] = useState<LogFilter>('all');
  const [tail, setTail] = useState<{ status: 'idle' | 'loading' | 'ready' | 'error'; data?: InstanceLogTail; error?: string }>({ status: 'idle' });
  const sortedLogs = useMemo(() => sortLogs(logs, sort), [logs, sort]);

  useEffect(() => {
    if (!logs.length) {
      setSelected('');
      return;
    }
    if (!selected || !logs.some((log) => log.name === selected)) {
      setSelected(pickInitialLog(logs));
    }
  }, [logs, selected]);

  useEffect(() => {
    if (!selected) {
      setTail({ status: 'idle' });
      return;
    }
    let alive = true;
    const load = (showLoading: boolean): void => {
      if (showLoading) {
        setTail((current) => current.data?.name === selected ? current : { status: 'loading' });
      }
      void fetchLogTail(inst.id, selected)
        .then((data) => {
          if (alive) setTail({ status: 'ready', data });
        })
        .catch((err) => {
          if (alive) setTail({ status: 'error', error: errMessage(err) });
        });
    };
    load(true);
    const timer = running ? window.setInterval(() => load(false), LOG_TAIL_POLL_MS) : 0;
    return () => {
      alive = false;
      if (timer) window.clearInterval(timer);
    };
  }, [inst.id, running, selected]);

  return (
    <div class="cp-instance-body cp-logs-pane">
      <div class="cp-resource-toolbar cp-logs-toolbar">
        <strong>{logs.length} log file{logs.length === 1 ? '' : 's'}</strong>
        <div class="cp-logs-tools">
          <div class="cp-mini-seg" role="tablist" aria-label="Sort logs">
            {(Object.keys(LOG_SORT_LABELS) as LogSort[]).map((item) => (
              <button
                key={item}
                type="button"
                role="tab"
                aria-selected={sort === item}
                data-active={sort === item}
                onClick={() => setSort(item)}
              >
                {LOG_SORT_LABELS[item]}
              </button>
            ))}
          </div>
          <div class="cp-mini-seg" role="tablist" aria-label="Filter log lines">
            {(Object.keys(LOG_FILTER_LABELS) as LogFilter[]).map((item) => (
              <button
                key={item}
                type="button"
                role="tab"
                aria-selected={filter === item}
                data-active={filter === item}
                onClick={() => setFilter(item)}
              >
                {LOG_FILTER_LABELS[item]}
              </button>
            ))}
          </div>
          <Button variant="secondary" size="sm" icon="refresh" onClick={onRefresh}>Refresh</Button>
          <Button variant="soft" size="sm" icon="folder" onClick={() => void openInstanceFolder(inst.id, 'logs')}>Open logs</Button>
        </div>
      </div>
      <ResourceStatus state={resources} onRetry={onRefresh} />
      {logs.length === 0 && resources.status !== 'loading' ? (
        <ResourceEmpty icon="terminal" title="No logs yet" hint="Launch this instance and Minecraft log files will appear here." />
      ) : (
        <div class="cp-logs-layout">
          <div class="cp-logs-list">
            {sortedLogs.map((log) => (
              <button key={log.name} type="button" data-active={selected === log.name} onClick={() => setSelected(log.name)}>
                <span>{log.name}</span>
                <small>{isCurrentLog(log.name) ? 'Current/latest · ' : ''}{fmtBytes(log.size)} · {fmtRelative(log.modified_at)}</small>
              </button>
            ))}
          </div>
          <div class="cp-log-preview">
            {tail.status === 'loading' && <div class="cp-resource-note">Loading log preview…</div>}
            {tail.status === 'error' && <div class="cp-resource-note cp-resource-note--error">{tail.error}</div>}
            {tail.status === 'ready' && (
              <>
                {tail.data?.truncated && <div class="cp-log-truncated">Showing the last {fmtBytes(tail.data.size > 0 ? Math.min(tail.data.size, 128 * 1024) : 0)} of this log.</div>}
                <LogLines text={tail.data?.text ?? ''} filter={filter} />
              </>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

function ResourceToolbar({
  title,
  onRefresh,
  action,
}: {
  title: string;
  onRefresh: () => void;
  action: { icon: string; label: string; onClick: () => void };
}): JSX.Element {
  return (
    <div class="cp-resource-toolbar">
      <strong>{title}</strong>
      <div>
        <Button variant="secondary" size="sm" icon="refresh" onClick={onRefresh}>Refresh</Button>
        <Button variant="soft" size="sm" icon={action.icon} onClick={action.onClick}>{action.label}</Button>
      </div>
    </div>
  );
}

function ResourceEmpty({ icon, title, hint }: { icon: string; title: string; hint: string }): JSX.Element {
  return (
    <div class="cp-resource-empty">
      <span><Icon name={icon} size={20} /></span>
      <strong>{title}</strong>
      <p>{hint}</p>
    </div>
  );
}

function ResourceRow({
  icon,
  name,
  meta,
  actions,
}: {
  icon: string;
  name: string;
  meta: string;
  actions?: JSX.Element;
}): JSX.Element {
  return (
    <div class="cp-resource-row">
      <span class="cp-resource-row-icon"><Icon name={icon} size={15} /></span>
      <span class="cp-resource-name" title={name}>{name}</span>
      <span class="cp-resource-meta">{meta}</span>
      {actions ? <span class="cp-resource-actions">{actions}</span> : null}
    </div>
  );
}

type InstanceWindowPreset = { id: string; label: string; w: number; h: number };

const WINDOW_PRESETS: InstanceWindowPreset[] = [
  { id: 'default', label: 'Default', w: 854, h: 480 },
  { id: 'hd', label: '720p', w: 1280, h: 720 },
  { id: 'fhd', label: '1080p', w: 1920, h: 1080 },
  { id: '2k', label: '2K', w: 2560, h: 1440 },
];

const INSTANCE_PERFORMANCE_OPTIONS: Array<{ value: InstancePerformanceMode; label: string }> = [
  { value: '', label: 'Inherit' },
  { value: 'managed', label: 'Managed' },
  { value: 'vanilla', label: 'Vanilla' },
  { value: 'custom', label: 'Custom' },
];

function clampWindowDimension(value: string, fallback: number): number {
  const parsed = Number.parseInt(value, 10);
  if (!Number.isFinite(parsed)) return fallback;
  return Math.max(320, Math.min(3840, parsed));
}

function instancePerformanceModeFrom(value: string | undefined): InstancePerformanceMode {
  return performanceModeFrom(value) ?? '';
}

function SettingsPane({ inst }: { inst: EnrichedInstance }): JSX.Element {
  const initialArtSeed = artSeedFor(inst);
  const [artSeed, setArtSeed] = useState<number>(initialArtSeed);
  const artPreset = artPresetForSeed(artSeed);
  const [maxMem, setMaxMem] = useState<number>(memoryGb(inst.max_memory_mb, config.value?.max_memory_mb ?? 4096));
  const [minMem, setMinMem] = useState<number>(memoryGb(inst.min_memory_mb, config.value?.min_memory_mb ?? 1024));
  const [width, setWidth] = useState<number>(inst.window_width ?? 854);
  const [height, setHeight] = useState<number>(inst.window_height ?? 480);
  const [performanceMode, setPerformanceMode] = useState<InstancePerformanceMode>(instancePerformanceModeFrom(inst.performance_mode));
  const [jvmPreset, setJvmPreset] = useState<JvmPreset>(jvmPresetFrom(inst.jvm_preset));
  const [javaPath, setJavaPath] = useState<string>(inst.java_path ?? '');
  const [jvmArgs, setJvmArgs] = useState<string>(inst.extra_jvm_args ?? '');
  const [advancedOpen, setAdvancedOpen] = useState<boolean>(Boolean(inst.java_path || inst.extra_jvm_args));
  const [activeSettingsSection, setActiveSettingsSection] = useState<string>('policy');
  const [saving, setSaving] = useState(false);
  const totalGB = systemInfo.value?.total_memory_mb ? Math.max(1, Math.floor(systemInfo.value.total_memory_mb / 1024)) : 32;
  const ramMax = Math.max(2, Math.min(32, totalGB));
  const rec = getMemoryRecommendation(totalGB);
  const recMin = Math.min(ramMax, Math.max(1, rec.rec - 2));
  const recMax = Math.min(ramMax, rec.rec + 2);
  const memoryZones: SliderZone[] = [
    { from: 0.5, to: recMin, tone: 'low', label: 'Low' },
    { from: recMin, to: recMax, tone: 'sweet', label: 'Recommended' },
    { from: recMax, to: Math.min(ramMax, Math.max(recMax, ramMax * 0.75)), tone: 'high', label: 'High' },
    { from: Math.min(ramMax, Math.max(recMax, ramMax * 0.75)), to: ramMax, tone: 'extreme', label: 'Aggressive' },
  ];
  const activeWindowPreset = WINDOW_PRESETS.find(p => p.w === width && p.h === height)?.id ?? 'custom';
  const activeWindowLabel = WINDOW_PRESETS.find(p => p.id === activeWindowPreset)?.label ?? 'Custom';
  const effectiveSettingsMode = performanceMode || globalPerformanceMode();
  const performanceModeText = performanceMode
    ? `${performanceModeLabel(effectiveSettingsMode)} override`
    : `Inherits ${performanceModeLabel(effectiveSettingsMode)} from global settings`;
  const runtimePresetText = `${JVM_PRESET_LABELS[jvmPreset]}: ${JVM_PRESET_HINTS[jvmPreset]}`;
  const settingsSections = [
    { id: 'policy', label: 'Performance policy', meta: performanceModeText },
    { id: 'memory', label: 'Memory', meta: `${fmtMem(recMin)} to ${fmtMem(recMax)} recommended` },
    { id: 'runtime', label: 'Runtime', meta: runtimePresetText },
    { id: 'window', label: 'Window', meta: `${activeWindowLabel} · ${width} × ${height}` },
    { id: 'identity', label: 'Identity', meta: `${artPreset} artwork style` },
  ];
  const dirty = (
    artSeed !== initialArtSeed ||
    Math.round(maxMem * 1024) !== (inst.max_memory_mb ?? config.value?.max_memory_mb ?? 4096) ||
    Math.round(Math.min(minMem, maxMem) * 1024) !== (inst.min_memory_mb ?? config.value?.min_memory_mb ?? 1024) ||
    width !== (inst.window_width ?? 854) ||
    height !== (inst.window_height ?? 480) ||
    performanceMode !== instancePerformanceModeFrom(inst.performance_mode) ||
    jvmPreset !== jvmPresetFrom(inst.jvm_preset) ||
    javaPath !== (inst.java_path ?? '') ||
    jvmArgs !== (inst.extra_jvm_args ?? '')
  );

  useEffect(() => {
    setMinMem(prev => Math.min(prev, maxMem));
  }, [maxMem]);

  useEffect(() => {
    const nextSeed = artSeedFor(inst);
    setArtSeed(nextSeed);
    setMaxMem(memoryGb(inst.max_memory_mb, config.value?.max_memory_mb ?? 4096));
    setMinMem(memoryGb(inst.min_memory_mb, config.value?.min_memory_mb ?? 1024));
    setWidth(inst.window_width ?? 854);
    setHeight(inst.window_height ?? 480);
    setPerformanceMode(instancePerformanceModeFrom(inst.performance_mode));
    setJvmPreset(jvmPresetFrom(inst.jvm_preset));
    setJavaPath(inst.java_path ?? '');
    setJvmArgs(inst.extra_jvm_args ?? '');
    setAdvancedOpen(Boolean(inst.java_path || inst.extra_jvm_args));
  }, [
    inst.id,
    inst.art_seed,
    inst.max_memory_mb,
    inst.min_memory_mb,
    inst.window_width,
    inst.window_height,
    inst.performance_mode,
    inst.jvm_preset,
    inst.java_path,
    inst.extra_jvm_args,
  ]);

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
        performance_mode: performanceMode,
        jvm_preset: jvmPreset,
        java_path: javaPath,
        extra_jvm_args: jvmArgs,
      });
      if (res?.error) throw new Error(res.error);
      updateInstanceInList(res);
      toast('Saved instance settings');
    } catch (err) {
      toast(`Could not save instance settings: ${errMessage(err)}`, 'error');
    } finally {
      setSaving(false);
    }
  };

  const jumpToSettingsSection = (sectionId: string): void => {
    setActiveSettingsSection(sectionId);
    document.getElementById(`cp-settings-${sectionId}`)?.scrollIntoView({ block: 'start' });
  };

  return (
    <div class="cp-instance-body cp-settings-pane">
      <div class="cp-resource-toolbar cp-settings-toolbar">
        <strong>Launch profile</strong>
        <div class="cp-settings-save">
          <span data-dirty={dirty}>{dirty ? 'Unsaved changes' : 'Up to date'}</span>
          <Button onClick={save} disabled={saving || !dirty} sound="affirm">{saving ? 'Saving…' : 'Save settings'}</Button>
        </div>
      </div>

      <div class="cp-logs-layout cp-settings-layout">
        <div class="cp-logs-list cp-settings-list" aria-label="Settings sections">
          {settingsSections.map((section) => (
            <button
              key={section.id}
              type="button"
              data-active={activeSettingsSection === section.id}
              onClick={() => jumpToSettingsSection(section.id)}
            >
              <span>{section.label}</span>
              <small>{section.meta}</small>
            </button>
          ))}
        </div>

        <div class="cp-log-preview cp-settings-preview">
          <div class="cp-settings-sheet">
        <section id="cp-settings-policy" class="cp-settings-row">
          <div class="cp-settings-row-head">
            <span class="cp-settings-section-icon"><Icon name="shield-check" size={15} /></span>
            <div>
              <h3>Performance policy</h3>
              <p>{performanceModeText}.</p>
            </div>
          </div>
          <div class="cp-settings-row-control">
            <div class="cp-settings-button-strip" aria-label="Instance performance mode">
              {INSTANCE_PERFORMANCE_OPTIONS.map((option) => (
                <Button
                  key={option.value || 'inherit'}
                  variant={performanceMode === option.value ? 'primary' : 'secondary'}
                  size="sm"
                  onClick={() => setPerformanceMode(option.value)}
                >
                  {option.label}
                </Button>
              ))}
            </div>
            <div class="cp-settings-mode-note">
              {performanceMode
                ? 'This instance will use its own performance mode.'
                : 'This instance follows the global Performance setting.'}
            </div>
          </div>
        </section>

        <section id="cp-settings-memory" class="cp-settings-row">
          <div class="cp-settings-row-head">
            <span class="cp-settings-section-icon"><Icon name="settings" size={15} /></span>
            <div>
              <h3>Memory</h3>
              <p>Recommended range: {fmtMem(recMin)} to {fmtMem(recMax)}.</p>
            </div>
          </div>
          <div class="cp-settings-row-control">
            <div class="cp-settings-memory-grid">
              <div class="cp-settings-slider-row">
                <div class="cp-settings-slider-label">
                  <span>Maximum heap</span>
                  <strong>{fmtMem(maxMem)}</strong>
                </div>
                <Slider
                  value={maxMem}
                  min={1}
                  max={ramMax}
                  step={0.5}
                  zones={memoryZones}
                  sound="memory"
                  onChange={setMaxMem}
                  ariaLabel="Maximum heap in gigabytes"
                />
              </div>
              <div class="cp-settings-slider-row">
                <div class="cp-settings-slider-label">
                  <span>Minimum heap</span>
                  <strong>{fmtMem(minMem)}</strong>
                </div>
                <Slider
                  value={minMem}
                  min={0.5}
                  max={maxMem}
                  step={0.5}
                  sound="memory"
                  onChange={setMinMem}
                  ariaLabel="Minimum heap in gigabytes"
                />
              </div>
            </div>
          </div>
        </section>

        <section id="cp-settings-runtime" class="cp-settings-row">
          <div class="cp-settings-row-head">
            <span class="cp-settings-section-icon"><Icon name="terminal" size={15} /></span>
            <div>
              <h3>Runtime</h3>
              <p>{runtimePresetText}</p>
            </div>
          </div>
          <div class="cp-settings-row-control">
            <div class="cp-settings-runtime-presets" role="radiogroup" aria-label="Runtime preset">
              {JVM_PRESET_ORDER.map((preset) => (
                <button
                  key={preset || 'auto'}
                  type="button"
                  role="radio"
                  aria-checked={jvmPreset === preset}
                  class="cp-settings-runtime-preset"
                  data-active={jvmPreset === preset}
                  onClick={() => setJvmPreset(preset)}
                  title={`${JVM_PRESET_LABELS[preset]}: ${JVM_PRESET_HINTS[preset]}`}
                >
                  <span class="cp-settings-runtime-preset-label">{JVM_PRESET_LABELS[preset]}</span>
                  <span class="cp-settings-runtime-preset-hint">{JVM_PRESET_HINTS[preset]}</span>
                </button>
              ))}
            </div>
            <div class="cp-settings-advanced-toggle">
              <Button
                variant="secondary"
                size="sm"
                icon={advancedOpen ? 'chevron-up' : 'chevron-down'}
                onClick={() => setAdvancedOpen(open => !open)}
              >
                Advanced overrides
              </Button>
            </div>
            {advancedOpen && (
              <div class="cp-settings-advanced-grid">
                <label>
                  <span>Java path</span>
                  <Input value={javaPath} onChange={setJavaPath} placeholder="Managed Java" />
                </label>
                <label>
                  <span>Extra JVM arguments</span>
                  <Input value={jvmArgs} onChange={setJvmArgs} placeholder="-Dfoo=bar -Xss2m" />
                </label>
              </div>
            )}
          </div>
        </section>

        <section id="cp-settings-window" class="cp-settings-row">
          <div class="cp-settings-row-head">
            <span class="cp-settings-section-icon"><Icon name="rectangle" size={15} /></span>
            <div>
              <h3>Window</h3>
              <p>{activeWindowLabel} · {width} × {height}</p>
            </div>
          </div>
          <div class="cp-settings-row-control cp-settings-window-control">
            <div class="cp-settings-button-strip" aria-label="Window size">
              {WINDOW_PRESETS.map((preset) => (
                <Button
                  key={preset.id}
                  variant={activeWindowPreset === preset.id ? 'primary' : 'secondary'}
                  size="sm"
                  onClick={() => {
                    setWidth(preset.w);
                    setHeight(preset.h);
                  }}
                >
                  {preset.label}
                </Button>
              ))}
            </div>
            <div class="cp-settings-dimensions">
              <label>
                <span>Width</span>
                <Input
                  type="number"
                  value={String(width)}
                  onChange={(v) => setWidth(clampWindowDimension(v, width))}
                />
              </label>
              <label>
                <span>Height</span>
                <Input
                  type="number"
                  value={String(height)}
                  onChange={(v) => setHeight(clampWindowDimension(v, height))}
                />
              </label>
            </div>
          </div>
        </section>

        <section id="cp-settings-identity" class="cp-settings-row cp-settings-row--identity">
          <div class="cp-settings-row-head">
            <span class="cp-settings-section-icon"><Icon name="image" size={15} /></span>
            <div>
              <h3>Identity</h3>
              <p>Artwork used for this instance.</p>
            </div>
          </div>
          <div class="cp-settings-row-control cp-settings-identity-control">
            <InstanceArt
              instance={{ ...inst, art_seed: artSeed }}
              aspect="square"
              radius={12}
              className="cp-settings-avatar"
            />
            <div>
              <strong>{artPreset}</strong>
              <span>Current style</span>
            </div>
            <Button variant="secondary" size="sm" icon="refresh" onClick={() => setArtSeed(seed => nextArtSeed(seed))}>
              Regenerate
            </Button>
          </div>
        </section>
          </div>
        </div>
      </div>
    </div>
  );
}


export function InstanceDetailView({ id }: { id: string }): JSX.Element {
  const theme = useTheme();
  const inst = instances.value.find(i => i.id === id) as EnrichedInstance | undefined;
  const [tab, setTab] = useState<Tab>('overview');
  const [resources, setResources] = useState<ResourceLoadState>({ status: 'loading', data: null });
  const running = inst ? !!runningSessions.value[inst.id] : false;
  const launch = launchState.value;
  const preparing = inst && launch.status === 'preparing' && launch.instanceId === inst.id ? launch : null;

  const reloadResources = (): void => {
    if (!inst) return;
    setResources((current) => ({ status: 'loading', data: current.data ?? null }));
    void fetchInstanceResources(inst.id)
      .then((data) => setResources({ status: 'ready', data }))
      .catch((err) => setResources((current) => ({
        status: 'error',
        data: current.data ?? null,
        error: errMessage(err),
      })));
  };

  useEffect(() => {
    if (!inst) return;
    let alive = true;
    setResources({ status: 'loading', data: null });
    void fetchInstanceResources(inst.id)
      .then((data) => {
        if (alive) setResources({ status: 'ready', data });
      })
      .catch((err) => {
        if (alive) setResources({ status: 'error', data: null, error: errMessage(err) });
      });
    return () => { alive = false; };
  }, [inst?.id]);

  useEffect(() => {
    if (!inst || !running) return;
    let alive = true;
    const refreshQuietly = (): void => {
      void fetchInstanceResources(inst.id)
        .then((data) => {
          if (alive) setResources({ status: 'ready', data });
        })
        .catch((err) => {
          if (alive) {
            setResources((current) => ({
              status: 'error',
              data: current.data ?? null,
              error: errMessage(err),
            }));
          }
        });
    };
    refreshQuietly();
    const timer = window.setInterval(refreshQuietly, LOG_RESOURCE_POLL_MS);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [inst?.id, running]);

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
  const canLaunch = Boolean(v?.launchable);
  const installTarget = installTargetFor(inst, v);
  const install = installState.value;
  const installProgress = install.status === 'active' && install.versionId === installTarget
    ? { pct: install.pct, label: install.label }
    : null;
  const installQueued = !installProgress && installQueue.value.some(item => item.versionId === installTarget);
  const installLocked = !canLaunch && (Boolean(installProgress) || installQueued);

  const onPlay = (): void => {
    selectInstance(inst.id);
    void launchGame();
  };
  const onInstall = (): void => {
    selectInstance(inst.id);
    handleInstallClick();
  };
  const onStop = (): void => {
    selectInstance(inst.id);
    void killGame();
  };

  const tabCount = (t: Tab): number | undefined => {
    if (t === 'mods') {
      const n = resources.data?.mods_count ?? inst.mods_count ?? 0;
      return n > 0 ? n : undefined;
    }
    if (t === 'worlds') {
      const n = resources.data?.worlds_count ?? inst.saves_count ?? 0;
      return n > 0 ? n : undefined;
    }
    if (t === 'screenshots') {
      const n = resources.data?.screenshots_count ?? 0;
      return n > 0 ? n : undefined;
    }
    if (t === 'logs') {
      const n = resources.data?.logs_count ?? 0;
      return n > 0 ? n : undefined;
    }
    return undefined;
  };

  const loaderVer = v?.loader?.loader_version ?? '';
  const launchNotice = launchNotices.value[inst.id];

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
                  canLaunch={canLaunch}
                  installQueued={installQueued}
                  installProgress={installProgress}
                  onLaunch={onPlay}
                  onInstall={onInstall}
                  onOpenLogs={() => setTab('logs')}
                  onOpenSettings={() => setTab('settings')}
                  preparing={preparing}
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
                { icon: 'folder', label: 'Open resource packs folder', onSelect: () => void openInstanceFolder(inst.id, 'resourcepacks') },
                { icon: 'folder', label: 'Open shader packs folder', onSelect: () => void openInstanceFolder(inst.id, 'shaderpacks') },
                { icon: 'copy', label: 'Duplicate', onSelect: () => void duplicateInstance(inst) },
                { icon: 'edit', label: 'Rename', onSelect: () => void renameInstance(inst) },
                { label: '', onSelect: () => {}, divider: true },
                { icon: 'trash', label: 'Delete', onSelect: () => void deleteInstanceFlow(inst, () => navigate({ name: 'instances' })), danger: true },
              ])} />
          </div>
        </div>
      </div>

      {!installLocked && (
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
      )}

      {launchNotice && <LaunchOutcomeNotice inst={inst} notice={launchNotice} />}

      {installLocked && (
        <InstallBarrierPane
          installTarget={installTarget}
          installQueued={installQueued}
          installProgress={installProgress}
        />
      )}
      {!installLocked && tab === 'overview' && (
        <>
          <OverviewPane
            inst={inst}
            resources={resources.data}
            running={running}
            onLaunch={onPlay}
            onStop={onStop}
            onOpenWorlds={() => setTab('worlds')}
            onOpenLogs={() => setTab('logs')}
          />
          <div class="cp-instance-bottom">
            <LogsCard inst={inst} resources={resources.data} running={running} onOpenLogs={() => setTab('logs')} />
          </div>
        </>
      )}
      {!installLocked && tab === 'mods' && <ModsPane inst={inst} resources={resources} onRefresh={reloadResources} />}
      {!installLocked && tab === 'worlds' && <WorldsPane inst={inst} resources={resources} onRefresh={reloadResources} />}
      {!installLocked && tab === 'screenshots' && <ScreenshotsPane inst={inst} resources={resources} onRefresh={reloadResources} />}
      {!installLocked && tab === 'logs' && <LogsPane inst={inst} resources={resources} running={running} onRefresh={reloadResources} />}
      {!installLocked && tab === 'settings' && <SettingsPane inst={inst} />}
    </div>
  );
}
