import { collapsedLogSeverity, currentPage, logLines } from './store';
import { toast } from './toast';
import type {
  CatalogVersion,
  LifecycleLabel,
  LifecycleMeta,
  LoaderBuildRecord,
  LoaderBuildMetadata,
  LoaderType,
  Page,
  Version,
} from './types';

import type { LogSeverity } from './store';

export function cn(...inputs: unknown[]): string {
  return inputs.filter((v): v is string => typeof v === 'string' && v.length > 0).join(' ');
}

const SEVERITY_RANK: Record<LogSeverity, number> = { error: 3, system: 2, info: 1 };

function logSeverityFromSource(source: string): LogSeverity {
  if (source === 'stderr') return 'error';
  if (source === 'system') return 'system';
  return 'info';
}

function updateLogIndicator(source: string): void {
  const newSeverity = logSeverityFromSource(source);
  const currentSeverity = collapsedLogSeverity.value;
  if (currentSeverity && SEVERITY_RANK[currentSeverity] >= SEVERITY_RANK[newSeverity]) return;
  collapsedLogSeverity.value = newSeverity;
}

export function appendLog(source: string, text: string, instanceId?: string, instanceName?: string): void {
  void text;
  void instanceId;
  void instanceName;
  logLines.value += 1;
  updateLogIndicator(source);
}

export function showError(msg: string): void {
  appendLog('stderr', `ERROR: ${msg}`);
  toast(msg, 'error');
}

export function errMessage(err: unknown): string {
  if (err instanceof Error && err.message) return err.message;
  if (typeof err === 'string') return err;
  return 'Unknown error';
}

interface VersionDisplay {
  name: string;
  hint: string | null;
  loader?: string | null;
}

type VersionLike =
  | Pick<Version, 'id' | 'inherits_from' | 'loader' | 'minecraft_meta' | 'lifecycle' | 'release_time'>
  | Pick<CatalogVersion, 'id' | 'minecraft_meta' | 'lifecycle' | 'release_time'>;

export function hasLifecycleLabel(lifecycle: LifecycleMeta | undefined | null, label: LifecycleLabel): boolean {
  return !!lifecycle?.labels?.includes(label);
}

export function isReleaseVersion(
  version: Pick<Version, 'lifecycle'> | Pick<CatalogVersion, 'lifecycle'> | null | undefined,
): boolean {
  return version?.lifecycle?.channel === 'stable' && hasLifecycleLabel(version.lifecycle, 'release');
}

export function isSnapshotVersion(
  version: Pick<Version, 'lifecycle'> | Pick<CatalogVersion, 'lifecycle'> | null | undefined,
): boolean {
  if (!version?.lifecycle) return false;
  if (hasLifecycleLabel(version.lifecycle, 'old_beta') || hasLifecycleLabel(version.lifecycle, 'old_alpha')) {
    return false;
  }
  return version.lifecycle.channel === 'preview' || version.lifecycle.channel === 'experimental';
}

export function isOldBetaVersion(
  version: Pick<Version, 'lifecycle'> | Pick<CatalogVersion, 'lifecycle'> | null | undefined,
): boolean {
  return hasLifecycleLabel(version?.lifecycle, 'old_beta');
}

export function isOldAlphaVersion(
  version: Pick<Version, 'lifecycle'> | Pick<CatalogVersion, 'lifecycle'> | null | undefined,
): boolean {
  return hasLifecycleLabel(version?.lifecycle, 'old_alpha');
}

export function supportsMods(version: Pick<Version, 'loader'> | null | undefined): boolean {
  return !!version?.loader;
}

export function matchesVersionFilter(
  version: Pick<CatalogVersion, 'lifecycle'> | Pick<Version, 'lifecycle'>,
  filter: string,
): boolean {
  if (filter === 'release') return isReleaseVersion(version);
  if (filter === 'snapshot') return isSnapshotVersion(version);
  if (filter === 'old_beta') return isOldBetaVersion(version);
  if (filter === 'old_alpha') return isOldAlphaVersion(version);
  return true;
}

export function versionBadgeInfo(version: Version | null | undefined): { cls: string; text: string } {
  if (isReleaseVersion(version)) return { cls: 'badge-release', text: 'REL' };
  if (isSnapshotVersion(version)) return { cls: 'badge-snapshot', text: 'SNAP' };
  if (isOldBetaVersion(version)) return { cls: 'badge-old', text: 'BETA' };
  if (isOldAlphaVersion(version)) return { cls: 'badge-old', text: 'ALPH' };
  return { cls: 'badge-old', text: version?.lifecycle?.badge_text || '?' };
}

export function parseVersionDisplay(versionId: string, version: VersionLike | null | undefined): VersionDisplay {
  if (version && 'inherits_from' in version && version.inherits_from) {
    return parseModded(versionId, version.inherits_from, version as Version | null);
  }
  if (version?.minecraft_meta?.display_name) {
    return {
      name: version.minecraft_meta.display_name,
      hint: version.minecraft_meta.display_hint || null,
    };
  }
  if (isOldBetaVersion(version)) return { name: versionId.replace(/^b/, 'Beta '), hint: null };
  if (isOldAlphaVersion(version)) return { name: versionId.replace(/^a/, 'Alpha '), hint: null };
  return { name: versionId, hint: null };
}

function loaderTermTags(buildMeta: LoaderBuildMetadata | undefined | null): string[] {
  return buildMeta?.display_tags ?? [];
}

export function formatLoaderBuildLabel(build: Pick<LoaderBuildRecord, 'loader_version' | 'build_meta'>): string {
  const tags = loaderTermTags(build.build_meta);
  return tags.length > 0 ? `${build.loader_version} (${tags.join(', ')})` : build.loader_version;
}

export function formatLoaderVersionLabel(loaderVersion: string, buildMeta?: LoaderBuildMetadata | null): string {
  const tags = loaderTermTags(buildMeta);
  return tags.length > 0 ? `${loaderVersion} (${tags.join(', ')})` : loaderVersion;
}

function parseModded(id: string, base: string, version?: Version | null): VersionDisplay {
  const normalized = parseNormalizedLoaderDisplay(base, version);
  if (normalized) return normalized;

  const lo = id.toLowerCase();
  if (lo.startsWith('fabric-loader-')) {
    const suffix = base ? `-${base}` : '';
    const rest = id.slice('fabric-loader-'.length);
    const loaderVersion = suffix && rest.endsWith(suffix) ? rest.slice(0, -suffix.length) : rest;
    return loaderDisplay('fabric', base, loaderVersion);
  }
  if (lo.startsWith('quilt-loader-')) {
    const suffix = base ? `-${base}` : '';
    const rest = id.slice('quilt-loader-'.length);
    const loaderVersion = suffix && rest.endsWith(suffix) ? rest.slice(0, -suffix.length) : rest;
    return loaderDisplay('quilt', base, loaderVersion);
  }
  const forgeIndex = lo.lastIndexOf('-forge-');
  if (forgeIndex > 0) {
    return loaderDisplay('forge', base, id.slice(forgeIndex + '-forge-'.length));
  }
  if (lo.startsWith('neoforge-')) {
    return loaderDisplay('neoforge', base, id.slice('neoforge-'.length));
  }
  const m = id.match(/-optifine[_-](.*)/i);
  if (m) return { name: `OptiFine ${base}`, hint: m[1].replace(/_/g, ' ').trim(), loader: null };
  if (lo.includes('fabric')) return { name: `Fabric ${base}`, hint: null, loader: 'fabric' };
  if (lo.includes('quilt')) return { name: `Quilt ${base}`, hint: null, loader: 'quilt' };
  if (lo.includes('liteloader')) return { name: `LiteLoader ${base}`, hint: null, loader: null };
  return { name: base, hint: id !== base ? id : null, loader: null };
}

function parseNormalizedLoaderDisplay(base: string, version?: Version | null): VersionDisplay | null {
  if (!version?.loader) return null;
  const loader = loaderTypeFromComponentId(version.loader.component_id);
  if (!loader) return null;
  return loaderDisplay(loader, base, version.loader.loader_version, version.loader.build_meta);
}

function loaderTypeFromComponentId(componentId: string): LoaderType | null {
  if (componentId === 'net.fabricmc.fabric-loader') return 'fabric';
  if (componentId === 'org.quiltmc.quilt-loader') return 'quilt';
  if (componentId === 'net.minecraftforge') return 'forge';
  if (componentId === 'net.neoforged') return 'neoforge';
  return null;
}

function loaderDisplay(
  loader: LoaderType,
  base: string,
  loaderVersion: string,
  buildMeta?: LoaderBuildMetadata | null,
): VersionDisplay {
  const title =
    loader === 'fabric'
      ? `Fabric ${base}`
      : loader === 'quilt'
        ? `Quilt ${base}`
        : loader === 'forge'
          ? `Forge ${base}`
          : `NeoForge ${base}`;
  const hintPrefix = loader === 'fabric' || loader === 'quilt' ? 'Loader' : loader === 'forge' ? 'Forge' : 'NeoForge';
  return {
    name: title,
    hint: loaderVersion ? `${hintPrefix} ${formatLoaderVersionLabel(loaderVersion, buildMeta)}` : null,
    loader,
  };
}

export function fmtMem(gb: number): string {
  return gb === Math.floor(gb) ? `${gb}\u00A0GB` : `${gb.toFixed(1)}\u00A0GB`;
}

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

export const USERNAME_MIN_LEN = 3;
export const USERNAME_MAX_LEN = 16;
export const USERNAME_PATTERN: RegExp = /^[A-Za-z0-9_]+$/;

export function validateUsername(raw: string): string | null {
  const v = raw.trim();
  if (v.length === 0) return 'Enter a name.';
  if (v.length < USERNAME_MIN_LEN) return `At least ${USERNAME_MIN_LEN} characters.`;
  if (v.length > USERNAME_MAX_LEN) return `At most ${USERNAME_MAX_LEN} characters.`;
  if (!USERNAME_PATTERN.test(v)) return 'Letters, numbers, and underscores only.';
  return null;
}

export function getMemoryRecommendation(totalGB: number): { rec: number; text: string } {
  if (totalGB <= 4) return { rec: 2, text: 'Low RAM: 2 GB recommended' };
  if (totalGB <= 8) return { rec: 4, text: '4 GB recommended' };
  if (totalGB <= 16) return { rec: 6, text: '6 GB recommended' };
  return { rec: 8, text: '8 GB recommended' };
}

export function setPage(page: Page): void {
  currentPage.value = page;
}

export function toggleShortcutHints(show: boolean): void {
  document.body.classList.toggle('show-shortcuts', show);
}
