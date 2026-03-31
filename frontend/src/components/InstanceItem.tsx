import type { JSX } from 'preact';
import { useComputed } from '@preact/signals';
import type { Instance, Version } from '../types';
import {
  selectedInstanceId, runningSessions, installState, installQueue, versions,
} from '../store';
import { selectInstance } from '../actions';
import { parseVersionDisplay } from '../utils';

const KNOWN_LOADERS = new Set(['fabric', 'quilt', 'forge', 'neoforge']);

function LoaderIcon({ loader }: { loader: string }): JSX.Element | null {
  switch (loader) {
    case 'fabric':
      // Fabric pixel-art knot (cropped to content)
      return (
        <svg width="10" height="10" viewBox="2 1 13 14" shape-rendering="crispEdges">
          <rect x="8" y="2" width="1" height="1" fill="#38342a" />
          <rect x="9" y="2" width="1" height="1" fill="#dbd0b4" />
          <rect x="10" y="2" width="1" height="1" fill="#38342a" />
          <rect x="8" y="3" width="1" height="1" fill="#38342a" />
          <rect x="9" y="3" width="1" height="1" fill="#c6bca5" />
          <rect x="10" y="3" width="1" height="1" fill="#dbd0b4" />
          <rect x="11" y="3" width="1" height="1" fill="#38342a" />
          <rect x="7" y="4" width="1" height="1" fill="#38342a" />
          <rect x="8" y="4" width="1" height="1" fill="#dbd0b4" />
          <rect x="9" y="4" width="1" height="1" fill="#38342a" />
          <rect x="10" y="4" width="1" height="1" fill="#bcb29c" />
          <rect x="11" y="4" width="1" height="1" fill="#dbd0b4" />
          <rect x="12" y="4" width="1" height="1" fill="#38342a" />
          <rect x="6" y="5" width="1" height="1" fill="#38342a" />
          <rect x="7" y="5" width="1" height="1" fill="#c6bca5" />
          <rect x="8" y="5" width="2" height="1" fill="#dbd0b4" />
          <rect x="10" y="5" width="1" height="1" fill="#38342a" />
          <rect x="11" y="5" width="1" height="1" fill="#bcb29c" />
          <rect x="12" y="5" width="1" height="1" fill="#dbd0b4" />
          <rect x="13" y="5" width="1" height="1" fill="#38342a" />
          <rect x="5" y="6" width="1" height="1" fill="#38342a" />
          <rect x="6" y="6" width="2" height="1" fill="#dbd0b4" />
          <rect x="8" y="6" width="1" height="1" fill="#c6bca5" />
          <rect x="9" y="6" width="2" height="1" fill="#dbd0b4" />
          <rect x="11" y="6" width="1" height="1" fill="#38342a" />
          <rect x="12" y="6" width="1" height="1" fill="#bcb29c" />
          <rect x="13" y="6" width="2" height="1" fill="#38342a" />
          <rect x="4" y="7" width="1" height="1" fill="#38342a" />
          <rect x="5" y="7" width="4" height="1" fill="#dbd0b4" />
          <rect x="9" y="7" width="2" height="1" fill="#c6bca5" />
          <rect x="11" y="7" width="1" height="1" fill="#bcb29c" />
          <rect x="12" y="7" width="1" height="1" fill="#38342a" />
          <rect x="13" y="7" width="1" height="1" fill="#807a6d" />
          <rect x="14" y="7" width="1" height="1" fill="#38342a" />
          <rect x="3" y="8" width="1" height="1" fill="#38342a" />
          <rect x="4" y="8" width="2" height="1" fill="#dbd0b4" />
          <rect x="6" y="8" width="1" height="1" fill="#c6bca5" />
          <rect x="7" y="8" width="3" height="1" fill="#dbd0b4" />
          <rect x="10" y="8" width="2" height="1" fill="#bcb29c" />
          <rect x="12" y="8" width="2" height="1" fill="#38342a" />
          <rect x="2" y="9" width="1" height="1" fill="#38342a" />
          <rect x="3" y="9" width="1" height="1" fill="#aea694" />
          <rect x="4" y="9" width="3" height="1" fill="#dbd0b4" />
          <rect x="7" y="9" width="2" height="1" fill="#c6bca5" />
          <rect x="9" y="9" width="1" height="1" fill="#dbd0b4" />
          <rect x="10" y="9" width="1" height="1" fill="#bcb29c" />
          <rect x="11" y="9" width="1" height="1" fill="#38342a" />
          <rect x="2" y="10" width="1" height="1" fill="#38342a" />
          <rect x="3" y="10" width="1" height="1" fill="#9a927e" />
          <rect x="4" y="10" width="1" height="1" fill="#aea694" />
          <rect x="5" y="10" width="3" height="1" fill="#dbd0b4" />
          <rect x="8" y="10" width="2" height="1" fill="#bcb29c" />
          <rect x="10" y="10" width="1" height="1" fill="#38342a" />
          <rect x="3" y="11" width="1" height="1" fill="#38342a" />
          <rect x="4" y="11" width="1" height="1" fill="#9a927e" />
          <rect x="5" y="11" width="1" height="1" fill="#aea694" />
          <rect x="6" y="11" width="2" height="1" fill="#dbd0b4" />
          <rect x="8" y="11" width="1" height="1" fill="#bcb29c" />
          <rect x="9" y="11" width="1" height="1" fill="#38342a" />
          <rect x="4" y="12" width="1" height="1" fill="#38342a" />
          <rect x="5" y="12" width="1" height="1" fill="#9a927e" />
          <rect x="6" y="12" width="1" height="1" fill="#aea694" />
          <rect x="7" y="12" width="1" height="1" fill="#bcb29c" />
          <rect x="8" y="12" width="1" height="1" fill="#38342a" />
          <rect x="5" y="13" width="1" height="1" fill="#38342a" />
          <rect x="6" y="13" width="1" height="1" fill="#9a927e" />
          <rect x="7" y="13" width="2" height="1" fill="#38342a" />
          <rect x="6" y="14" width="2" height="1" fill="#38342a" />
        </svg>
      );
    case 'quilt':
      // Quilt patchwork (3x3 grid, bottom-right rotated)
      return (
        <svg width="10" height="10" viewBox="30 30 860 860" fill="white">
          <g transform="translate(-15, 0)">
            <path d="M249.16,128.54a15.28,15.28,0,0,0,15.26,15.27h26v58h-26a15.27,15.27,0,0,0,0,30.53h26v44.5a13.52,13.52,0,0,1-13.52,13.53h-44.5v-26a15.27,15.27,0,0,0-30.54,0v26h-58v-26a15.27,15.27,0,1,0-30.53,0v26H68.77a13.54,13.54,0,0,1-13.53-13.53V68.78A13.53,13.53,0,0,1,68.77,55.25H276.86a13.52,13.52,0,0,1,13.52,13.53v44.48h-26A15.27,15.27,0,0,0,249.16,128.54Z" />
            <path d="M514.84,128.54a15.27,15.27,0,0,0,15.26,15.27h26v58h-26a15.27,15.27,0,0,0,0,30.53h26v44.5a13.53,13.53,0,0,1-13.52,13.53H334.45a13.52,13.52,0,0,1-13.52-13.53v-44.5h25.94a15.27,15.27,0,1,0,0-30.53H320.93v-58h25.94a15.28,15.28,0,1,0,0-30.55H320.93V68.78a13.51,13.51,0,0,1,13.52-13.53H542.54a13.52,13.52,0,0,1,13.52,13.53v44.48h-26A15.27,15.27,0,0,0,514.84,128.54Z" />
            <path d="M821.73,68.78V276.86a13.52,13.52,0,0,1-13.52,13.53h-44.5v-26a15.27,15.27,0,1,0-30.53,0v26h-58v-26a15.27,15.27,0,1,0-30.53,0v26h-44.5a13.54,13.54,0,0,1-13.53-13.53v-44.5h26a15.27,15.27,0,1,0,0-30.53h-26v-58h26a15.28,15.28,0,0,0,0-30.55h-26V68.78a13.53,13.53,0,0,1,13.53-13.53H808.21A13.52,13.52,0,0,1,821.73,68.78Z" />
            <path d="M290.38,334.44V542.53a13.52,13.52,0,0,1-13.52,13.53h-44.5v-26a15.27,15.27,0,0,0-30.54,0v26h-58v-26a15.27,15.27,0,1,0-30.53,0v26H68.77a13.53,13.53,0,0,1-13.53-13.53V334.44a13.54,13.54,0,0,1,13.53-13.52h44.5v26a15.27,15.27,0,0,0,30.53,0v-26h58v26a15.27,15.27,0,0,0,30.54,0v-26h44.5A13.52,13.52,0,0,1,290.38,334.44Z" />
            <path d="M514.84,394.21a15.25,15.25,0,0,0,15.26,15.26h26v58h-26a15.27,15.27,0,0,0,0,30.53h26v44.5a13.52,13.52,0,0,1-13.52,13.53H498v-26a15.27,15.27,0,1,0-30.53,0v26h-58v-26a15.27,15.27,0,1,0-30.54,0v26H334.45a13.51,13.51,0,0,1-13.52-13.53V334.44a13.52,13.52,0,0,1,13.52-13.52H542.54a13.53,13.53,0,0,1,13.52,13.52v44.5h-26A15.26,15.26,0,0,0,514.84,394.21Z" />
            <path d="M821.73,334.44V542.53a13.52,13.52,0,0,1-13.52,13.53H600.12a13.53,13.53,0,0,1-13.53-13.53V498h26a15.27,15.27,0,1,0,0-30.53h-26v-58h26a15.27,15.27,0,1,0,0-30.53h-26v-44.5a13.54,13.54,0,0,1,13.53-13.52h44.5v26a15.27,15.27,0,0,0,30.53,0v-26h58v26a15.27,15.27,0,0,0,30.53,0v-26h44.5A13.52,13.52,0,0,1,821.73,334.44Z" />
            <path d="M249.16,659.89a15.28,15.28,0,0,0,15.26,15.27h26v58h-26a15.27,15.27,0,0,0,0,30.54h26v44.5a13.52,13.52,0,0,1-13.52,13.53H68.77a13.54,13.54,0,0,1-13.53-13.53V600.13A13.53,13.53,0,0,1,68.77,586.6h44.5v25.95a15.27,15.27,0,1,0,30.53,0V586.6h58v25.95a15.27,15.27,0,1,0,30.54,0V586.6h44.5a13.52,13.52,0,0,1,13.52,13.53v44.48h-26A15.28,15.28,0,0,0,249.16,659.89Z" />
            <path d="M556.06,600.13V808.21a13.53,13.53,0,0,1-13.52,13.53H334.45a13.52,13.52,0,0,1-13.52-13.53v-44.5h25.94a15.27,15.27,0,1,0,0-30.54H320.93v-58h25.94a15.28,15.28,0,1,0,0-30.55H320.93V600.13a13.51,13.51,0,0,1,13.52-13.53h44.49v25.95a15.27,15.27,0,1,0,30.54,0V586.6h58v25.95a15.27,15.27,0,1,0,30.53,0V586.6h44.5A13.52,13.52,0,0,1,556.06,600.13Z" />
            <rect x="635.3" y="635.29" width="235.14" height="235.14" rx="13.53" transform="translate(-311.85 752.86) rotate(-45)" />
          </g>
        </svg>
      );
    case 'forge':
      // Forge anvil shape
      return (
        <svg width="10" height="10" viewBox="0 10 100 60" fill="currentColor">
          <g transform="translate(-6, 16.7)">
            <path d="M91.6,16.7l-37.8-1.9l46.2,0v-3.7H47.8l0,7.8v6.2c0,0.1-1.5-9.1-1.9-11.7h-4.1v6.8v6.2 c0,0.1-1.8-10.9-1.9-12.3c-10.4,0-27.9,0-27.9,0c1.9,1.6,12.4,10.6,19.9,14.3c3.7,1.8,8.3,1.9,12.4,2c2.1,0.1,4.2,0.2,5.8,1.8 c2.3,2.2,2.8,5.7,0.8,8.3c-1.9,2.6-7.3,3.2-7.3,3.2L39,49.1v6.4h10.3l0.3-6.3l8.9-6.3c-0.9,0.8-3.1,2.8-6.2,7.7 c-0.7,1.1-1.3,2.3-1.7,3.5c2.2-1.9,6.8-3.2,12.2-3.2c5.3,0,9.9,1.3,12.1,3.2c-0.4-1.2-1-2.4-1.7-3.5c-3.2-4.9-5.3-6.9-6.2-7.7 l8.9,6.3l0.3,6.3h9.6v-6.4l-4.5-5.5c0,0-6.7-0.4-8.4-3.2C67.7,32.6,74.8,20.4,91.6,16.7z" />
          </g>
        </svg>
      );
    case 'neoforge':
      // NeoForge fox head (background removed, fox only)
      return (
        <svg width="10" height="10" viewBox="18 14 92 98" fill="currentColor">
          <g id="fox-head">
            <path fill="#262a33" d="M42 66h7v7H42ZM77 66h7v7H77Z" />
            <path fill="#8d7168" d="M42 23h7v7H42ZM77 23h7v7H77ZM35 30h7v8H35ZM84 30h7v8H84Z" />
            <path fill="#e68c37" d="M56 45h14v7H56ZM56 52h14v7H56ZM56 59h7v7H56Z" />
            <path fill="#66534d" d="M42 16h14v7H42ZM70 16h14v7H70ZM35 23h7v7H35ZM49 23h7v7H49ZM70 23h7v7H70ZM84 23h7v7H84ZM27 30h8v8H27ZM49 30h7v8H49ZM70 30h7v8H70ZM91 30h8v8H91ZM35 38h7v7H35ZM84 38h7v7H84Z" />
            <path fill="#c7a3b9" d="M42 38h7v7H42ZM77 38h7v7H77Z" />
            <path fill="#bf6134" d="M56 38h14v7H56ZM35 45h14v7H35ZM77 45h14v7H77ZM27 52h8v7H27ZM91 52h8v7H91ZM20 59h7v7H20ZM99 59h7v7H99ZM27 66h8v7H27ZM91 66h8v7H91ZM56 73h14v7H56ZM35 87h14v8H35ZM77 87h14v8H77Z" />
            <path fill="#a44e37" d="M56 30h14v8H56ZM27 38h8v7H27ZM49 38h7v7H49ZM70 38h7v7H70ZM91 38h8v7H91ZM27 45h8v7H27ZM91 45h8v7H91ZM20 73h7v7H20ZM99 73h7v7H99ZM27 80h8v7H27ZM91 80h8v7H91ZM27 87h8v8H27ZM91 87h8v8H91ZM35 95h14v7H35ZM77 95h14v7H77ZM49 102h28v7H49Z" />
            <path fill="#13151a" d="M42 73h7v7H42ZM77 73h7v7H77ZM56 87h14v8H56Z" />
            <path fill="#e7d9d3" d="M42 30h7v8H42ZM77 30h7v8H77ZM42 59h7v7H42ZM77 59h7v7H77ZM35 73h7v7H35ZM84 73h7v7H84ZM56 95h14v7H56Z" />
            <path fill="#f9f4f4" d="M35 59h7v7H35ZM84 59h7v7H84ZM35 66h7v7H35ZM84 66h7v7H84ZM49 87h7v8H49ZM70 87h7v8H70ZM49 95h7v7H49ZM70 95h7v7H70Z" />
            <path fill="#d7742f" d="M49 45h7v7H49ZM70 45h7v7H70ZM35 52h21v7H35ZM70 52h21v7H70ZM27 59h8v7H27ZM49 59h7v7H49ZM63 59h14v7H63ZM91 59h8v7H91ZM49 66h28v7H49ZM27 73h8v7H27ZM49 73h7v7H49ZM70 73h7v7H70ZM91 73h8v7H91ZM35 80h56v7H35Z" />
          </g>
        </svg>
      );
    default:
      return null;
  }
}

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

  const dotClass = useComputed(() =>
    isRunning.value ? 'running' : version?.launchable ? 'ok' : 'missing'
  );

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
        {p.loader ? (
          version?.inherits_from || p.name
        ) : p.hint ? (
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
