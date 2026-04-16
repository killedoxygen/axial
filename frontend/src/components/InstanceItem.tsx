import { useComputed } from '@preact/signals';
import type { Instance, Version } from '../types';
import {
  selectedInstanceId, runningSessions, installState, installQueue, versions, versionMap,
} from '../store';
import { selectInstance } from '../actions';
import { parseVersionDisplay } from '../utils';
import { LoaderIcon } from './LoaderIcons';

const KNOWN_LOADERS = new Set(['fabric', 'quilt', 'forge', 'neoforge']);

interface InstanceItemProps {
  instance: Instance;
  version: Version | undefined;
  index: number;
  onContextMenu: (e: MouseEvent, inst: Instance) => void;
}

export function InstanceItem({ instance, version, index, onContextMenu }: InstanceItemProps) {
  const isModded = !!version?.inherits_from;
  const pd = useComputed(() =>
    parseVersionDisplay(instance.version_id, version, versions.value)
  );

  const isRunning = useComputed(() => !!runningSessions.value[instance.id]);
  const isSelected = useComputed(() => selectedInstanceId.value === instance.id);

  const dotClass = useComputed(() => {
    if (isRunning.value) return 'running';
    const v = versionMap.value.get(instance.version_id);
    return v?.launchable ? 'ok' : 'missing';
  });

  const badgeClass = useComputed(() => {
    const p = pd.value;
    if (p.loader && KNOWN_LOADERS.has(p.loader)) return `badge-loader badge-${p.loader}`;
    if (isModded) return 'badge-modded';
    if (version?.type === 'release') return 'badge-release';
    if (version?.type === 'snapshot') return 'badge-snapshot';
    return 'badge-old';
  });

  const badgeText = useComputed(() => {
    const p = pd.value;
    if (p.loader && KNOWN_LOADERS.has(p.loader)) return null;
    if (isModded) return 'MOD';
    if (version?.type === 'release') return 'REL';
    if (version?.type === 'snapshot') return 'SNAP';
    if (version?.type === 'old_beta') return 'BETA';
    if (version?.type === 'old_alpha') return 'ALPH';
    return version?.type?.toUpperCase()?.slice(0, 4) || '?';
  });

  const installPct = useComputed(() => {
    const iTarget = version?.needs_install || version?.id || instance.version_id;
    const is = installState.value;
    if (is.status === 'active' && is.versionId === iTarget) return is.pct;
    if (installQueue.value.some(q => q.versionId === iTarget)) return 0;
    return -1;
  });

  const tooltip = useComputed(() => {
    const p = pd.value;
    if (!p.loader) return undefined;
    return p.hint ? `${p.name} \u2014 ${p.hint}` : p.name;
  });

  const handleClick = (e: MouseEvent) => {
    if (e.button !== 0) return;
    selectInstance(instance.id);
  };

  const handleContextMenu = (e: MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    selectInstance(instance.id);
    onContextMenu(e, instance);
  };

  const classes = [
    'version-item',
    version?.launchable ? '' : 'dimmed',
    isSelected.value ? 'selected' : '',
    isRunning.value ? 'is-running' : '',
  ].filter(Boolean).join(' ');

  const p = pd.value;
  const pctVal = installPct.value;
  const loaderType = p.loader && KNOWN_LOADERS.has(p.loader) ? p.loader : null;

  return (
    <button
      type="button"
      class={classes}
      data-id={instance.id}
      aria-pressed={isSelected.value ? 'true' : 'false'}
      aria-label={`Select instance ${instance.name}`}
      title={tooltip.value}
      style={{ animationDelay: `${index * 15}ms` }}
      onClick={(e: MouseEvent) => handleClick(e)}
      onContextMenu={(e: MouseEvent) => handleContextMenu(e)}
    >
      <div class={`version-dot ${dotClass.value}`} />
      <span class="version-name">{instance.name}</span>
      <span class="version-sub">
        {p.hint ? (
          <>
            {p.name} <span class="version-hint">{p.hint}</span>
          </>
        ) : (
          p.name
        )}
      </span>
      {isRunning.value && <span class="version-running-tag">LIVE</span>}
      <span class={`version-badge ${badgeClass.value}`}>
        {loaderType ? <LoaderIcon loader={loaderType} /> : badgeText.value}
      </span>
      {pctVal >= 0 && (
        <div class="version-install-bar">
          <div class="version-install-fill" style={{ width: `${pctVal}%` }} />
        </div>
      )}
    </button>
  );
}
