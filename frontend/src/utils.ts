import { byId } from './dom';
import { collapsedLogSeverity, currentPage, instances, logLines } from './store';
import type { Page } from './types';

const loggedInstances = new Set<string>();
let activeLogFilter = 'all';
import type { LogSeverity } from './store';

const SEVERITY_RANK: Record<LogSeverity, number> = { error: 3, system: 2, info: 1 };

function logSeverityFromSource(source: string): LogSeverity {
  if (source === 'stderr') return 'error';
  if (source === 'system') return 'system';
  return 'info';
}

function updateLogIndicator(source: string): void {
  const panel = byId<HTMLElement>('log-panel');
  if (panel?.classList.contains('expanded')) return;

  const newSeverity = logSeverityFromSource(source);
  const currentSeverity = collapsedLogSeverity.value;
  if (currentSeverity && SEVERITY_RANK[currentSeverity] >= SEVERITY_RANK[newSeverity]) return;
  collapsedLogSeverity.value = newSeverity;
}

export function clearLogIndicator(): void {
  collapsedLogSeverity.value = null;
}

function fmtTime(): string {
  const d = new Date();
  return `${String(d.getHours()).padStart(2,'0')}:${String(d.getMinutes()).padStart(2,'0')}:${String(d.getSeconds()).padStart(2,'0')}`;
}

export function appendLog(source: string, text: string, instanceId?: string, instanceName?: string): void {
  const logLinesEl = byId<HTMLElement>('log-lines');
  const logCountEl = byId<HTMLElement>('log-count');
  const logContentEl = byId<HTMLElement>('log-content');
  const line = document.createElement('div');
  line.className = `log-line ${source}`;
  if (instanceId) line.dataset.instance = instanceId;

  // Timestamp
  const ts = document.createElement('span');
  ts.className = 'log-ts';
  ts.textContent = fmtTime();
  line.appendChild(ts);

  // Instance tag
  if (instanceName && instanceId) {
    const tag = document.createElement('span');
    tag.className = 'log-tag';
    tag.textContent = instanceName;
    line.appendChild(tag);

    if (!loggedInstances.has(instanceId)) {
      loggedInstances.add(instanceId);
      if (loggedInstances.size > 1) logLinesEl?.classList.add('multi');
      syncLogFilter();
    }
  }

  line.appendChild(document.createTextNode(text));

  // Apply active filter
  if (activeLogFilter !== 'all' && instanceId && instanceId !== activeLogFilter) {
    line.classList.add('log-filtered');
  }

  logLinesEl?.appendChild(line);
  logLines.value += 1;
  if (logCountEl) logCountEl.textContent = `${logLines.value} line${logLines.value !== 1 ? 's' : ''}`;
  updateLogIndicator(source);
  if (logContentEl) logContentEl.scrollTop = logContentEl.scrollHeight;
}

export function setLogFilter(instanceId?: string): void {
  activeLogFilter = instanceId || 'all';
  const logLinesEl = byId<HTMLElement>('log-lines');
  const logContentEl = byId<HTMLElement>('log-content');
  if (!logLinesEl) return;
  const lines = logLinesEl.querySelectorAll('.log-line') as NodeListOf<HTMLElement>;
  for (const line of lines) {
    const lid = line.dataset.instance;
    if (activeLogFilter === 'all' || !lid || lid === activeLogFilter) {
      line.classList.remove('log-filtered');
    } else {
      line.classList.add('log-filtered');
    }
  }
  if (logContentEl) logContentEl.scrollTop = logContentEl.scrollHeight;
}

function syncLogFilter(): void {
  const filter = byId<HTMLSelectElement>('log-filter');
  if (!filter) return;
  // Rebuild filter options
  const current = filter.value;
  filter.replaceChildren(new Option('All instances', 'all'));
  for (const id of loggedInstances) {
    const inst = instances.value.find((instance) => instance.id === id);
    const name: string = inst?.name || id.slice(0, 8);
    const opt = document.createElement('option');
    opt.value = id;
    opt.textContent = name;
    filter.appendChild(opt);
  }
  filter.value = current || 'all';
  filter.classList.toggle('hidden', loggedInstances.size < 2);
}

export function showError(msg: string): void {
  appendLog('stderr', `ERROR: ${msg}`);
}

export function errMessage(err: unknown): string {
  if (err instanceof Error && err.message) return err.message;
  if (typeof err === 'string') return err;
  return 'Unknown error';
}

export function esc(s: string): string {
  const d = document.createElement('div');
  d.textContent = s;
  return d.innerHTML;
}

interface VersionDisplay {
  name: string;
  hint: string | null;
  loader?: string | null;
}

export function parseVersionDisplay(versionId: string, version: any, versions: any[]): VersionDisplay {
  if (version?.inherits_from) return parseModded(versionId, version.inherits_from);
  const type = version?.type;
  if (type === 'old_beta') return { name: versionId.replace(/^b/, 'Beta '), hint: null };
  if (type === 'old_alpha') return { name: versionId.replace(/^a/, 'Alpha '), hint: null };
  if (type === 'snapshot') return parseSnapshot(versionId, version, versions);
  return { name: versionId, hint: null };
}

function parseModded(id: string, base: string): VersionDisplay {
  const lo = id.toLowerCase();
  let m: RegExpMatchArray | null;
  // fabric-loader-0.16.9-1.20.1
  m = lo.match(/^fabric-loader-([.\d]+)-/);
  if (m) return { name: `Fabric ${base}`, hint: `Loader ${m[1]}`, loader: 'fabric' };
  // quilt-loader-0.26.1-1.20.1
  m = lo.match(/^quilt-loader-([.\d]+)-/);
  if (m) return { name: `Quilt ${base}`, hint: `Loader ${m[1]}`, loader: 'quilt' };
  // 1.20.1-forge-47.3.0 or 1.20.1-forge47.3.0
  m = id.match(/-forge-?([.\d]+)/i);
  if (m) return { name: `Forge ${base}`, hint: `Forge ${m[1]}`, loader: 'forge' };
  // neoforge variants
  if (lo.includes('neoforge')) {
    m = id.match(/neoforge[.-]?([.\d]+(?:-[.\d]+)?)/i);
    return { name: `NeoForge ${base}`, hint: m ? `NeoForge ${m[1]}` : null, loader: 'neoforge' };
  }
  // optifine
  m = id.match(/-optifine[_-](.*)/i);
  if (m) return { name: `OptiFine ${base}`, hint: m[1].replace(/_/g, ' ').trim(), loader: null };
  // X.X.X-fabric (simple)
  if (lo.includes('fabric')) return { name: `Fabric ${base}`, hint: null, loader: 'fabric' };
  if (lo.includes('quilt')) return { name: `Quilt ${base}`, hint: null, loader: 'quilt' };
  if (lo.includes('liteloader')) return { name: `LiteLoader ${base}`, hint: null, loader: null };
  // generic fallback
  return { name: base, hint: id !== base ? id : null, loader: null };
}

function parseSnapshot(id: string, version: any, versions: any[]): VersionDisplay {
  // pre-release / release candidate: 1.20.5-pre1, 1.20.5-rc1
  const m = id.match(/^(\d+\.\d+(?:\.\d+)?)-(?:pre|rc)\d+$/);
  if (m) return { name: id, hint: null };
  // weekly snapshot: find nearest release by time
  if (versions?.length && version?.release_time) {
    const t = version.release_time as string;
    const rel = versions.filter((v: any) => v.type === 'release' && v.release_time).sort((a: any, b: any) => (a.release_time as string).localeCompare(b.release_time as string));
    // first release at or after snapshot
    let nearest: any = null;
    for (const r of rel) { if (r.release_time >= t) { nearest = r; break; } }
    // if none after, use last release before
    if (!nearest) { for (let i = rel.length - 1; i >= 0; i--) { if (rel[i].release_time <= t) { nearest = rel[i]; break; } } }
    if (nearest && !id.includes(nearest.id)) return { name: id, hint: `~ ${nearest.id}` };
  }
  return { name: id, hint: null };
}

export function fmtMem(gb: number): string { return gb === Math.floor(gb) ? `${gb}\u00A0GB` : `${gb.toFixed(1)}\u00A0GB`; }

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return bytes + ' B';
  if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + ' KB';
  if (bytes < 1024 * 1024 * 1024) return (bytes / (1024 * 1024)).toFixed(1) + ' MB';
  return (bytes / (1024 * 1024 * 1024)).toFixed(2) + ' GB';
}

export function formatRelativeTime(date: Date): string {
  const now = new Date();
  const diff = now.getTime() - date.getTime();
  const mins = Math.floor(diff / 60000);
  if (mins < 1) return 'just now';
  if (mins < 60) return `${mins}m ago`;
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return `${hrs}h ago`;
  const days = Math.floor(hrs / 24);
  if (days < 7) return `${days}d ago`;
  return new Intl.DateTimeFormat(undefined, { dateStyle: 'medium' }).format(date);
}

export function getMemoryRecommendation(totalGB: number): { rec: number; text: string } {
  if (totalGB <= 4) return { rec: 2, text: 'Low RAM — 2 GB recommended' };
  if (totalGB <= 8) return { rec: 4, text: '4 GB recommended' };
  if (totalGB <= 16) return { rec: 6, text: '6 GB recommended' };
  return { rec: 8, text: '8 GB recommended' };
}

export function updateMemoryRecText(val: number, totalGB: number): void {
  const memoryRec = byId<HTMLElement>('memory-rec');
  if (!totalGB || !memoryRec) return;
  memoryRec.textContent = val < 2 ? '(low — may lag)' : val > totalGB * 0.75 ? '(high — leave room for OS)' : '';
}

export function setPage(page: Page): void {
  currentPage.value = page;
}

export function toggleShortcutHints(show: boolean): void {
  document.body.classList.toggle('show-shortcuts', show);
}
