import type { JSX } from 'preact';
import { useEffect, useLayoutEffect, useRef, useState } from 'preact/hooks';
import { Icon } from '../ui/Icons';
import { Chip, IconButton, Kbd, Ring } from '../ui/Atoms';
import { Face } from '../ui/Face';
import { WindowControls } from './WindowControls';
import { MusicWidget } from './MusicWidget';
import { UpdateWidget } from './UpdateWidget';
import { commandPaletteOpen, goBack, goForward, navigate, route } from '../ui-state';
import { shortcutHint } from '../shortcuts';
import { launchSessions, instances, versionById, launchState } from '../store';
import { activeDownload, downloadFailures, downloadQueue } from '../machines/downloads';
import { hasVisibleUpdate, updateFlow, updateFlowActive } from '../updater';
import { minecraftVersionLabel } from '../version-display';
import { hasCustomDragRegion, windowStartDragging, windowToggleMaximize } from '../native';
import { launchSessionActivityLabel, launchSessionIsPlaying } from '../launch-presenters';
import type { LaunchSession } from '../types-launch';

function assertUnreachable(value: never): never {
  throw new Error(`Unhandled route: ${JSON.stringify(value)}`);
}

function crumbsFor(): { label: string; onClick?: () => void }[] {
  const r = route.value;
  switch (r.name) {
    case 'home':
      return [{ label: 'Home' }];
    case 'instances':
      return [{ label: 'Instances' }];
    // A targeted Discover shows the instance it is adding to, so the trail says
    // where the content is headed, not just where you are.
    case 'discover': {
      const target = r.target ? instances.value.find((i) => i.id === r.target) : undefined;
      if (!target) return [{ label: 'Discover' }];
      return [
        { label: 'Instances', onClick: () => navigate({ name: 'instances' }) },
        { label: target.name, onClick: () => navigate({ name: 'instance', id: target.id }) },
        { label: 'Discover' },
      ];
    }
    case 'content': {
      const target = r.target ? instances.value.find((i) => i.id === r.target) : undefined;
      const trail = target
        ? [
            { label: 'Instances', onClick: () => navigate({ name: 'instances' }) },
            { label: target.name, onClick: () => navigate({ name: 'instance', id: target.id }) },
          ]
        : [];
      return [
        ...trail,
        { label: 'Discover', onClick: () => navigate({ name: 'discover', target: r.target }) },
        { label: 'Details' },
      ];
    }
    case 'instance': {
      const inst = instances.value.find((i) => i.id === r.id);
      return [
        { label: 'Instances', onClick: () => navigate({ name: 'instances' }) },
        { label: inst?.name || 'Instance' },
      ];
    }
    case 'dev-lab':
      return [{ label: 'Settings', onClick: () => navigate({ name: 'settings' }) }, { label: 'Dev lab' }];
    case 'downloads':
      return [{ label: 'Downloads' }];
    case 'accounts':
      return [{ label: 'Accounts & skins' }];
    case 'settings':
      return [{ label: 'Settings' }];
    default:
      return assertUnreachable(r);
  }
}

function versionTag(versionId: string | undefined): string | null {
  return minecraftVersionLabel(versionById(versionId), '') || null;
}

type GlyphState = 'preparing' | 'monitoring' | 'playing' | 'stopping' | 'downloading' | 'queued' | 'failed';

/** The face for how it stands, the ring while it is still going. */
function StatusGlyph({ state, progress }: { state: GlyphState; progress?: number }): JSX.Element {
  if (state === 'failed') return <Face eyes="x" />;
  if (state === 'queued') return <Face eyes="closed" />;
  if (state === 'playing') return <Face />;
  return <Ring progress={progress} />;
}

function sessionGlyph(session: Pick<LaunchSession, 'stopping' | 'viewModel'>): GlyphState {
  if (session.stopping) return 'stopping';
  if (launchSessionIsPlaying(session)) return 'playing';
  return 'monitoring';
}

function useSettledLabel(label: string | null, holdMs = 600): string | null {
  const [shown, setShown] = useState(label);
  const shownAt = useRef(performance.now());
  useEffect(() => {
    if (label === shown) return;
    const commit = (): void => {
      shownAt.current = performance.now();
      setShown(label);
    };
    if (label == null || shown == null) {
      commit();
      return;
    }
    const wait = Math.max(0, holdMs - (performance.now() - shownAt.current));
    if (wait === 0) {
      commit();
      return;
    }
    const t = window.setTimeout(commit, wait);
    return () => window.clearTimeout(t);
  }, [label, shown, holdMs]);
  return shown;
}

function StatusPill(): JSX.Element | null {
  const sessions = launchSessions.value;
  const install = activeDownload.value;

  const runIds = Object.keys(sessions);
  const inst = runIds.length > 0 ? instances.value.find((i) => i.id === runIds[0]) : null;
  const session = runIds.length > 0 ? sessions[runIds[0]] : null;

  let glyph: GlyphState | null = null;
  let tone: 'accent' | 'err' | undefined = 'accent';
  let progress: number | undefined;
  let onClick: (() => void) | undefined;
  let title: string | undefined;
  let ariaLabel: string | undefined;
  let content: JSX.Element | null = null;

  const launch = launchState.value;
  const queueState = downloadQueue.value;
  const failures = downloadFailures.value;
  const failure = failures[0];

  const rawFlowLabel =
    inst && session
      ? session.stopping
        ? 'Stopping'
        : launchSessionActivityLabel(session)
      : !install && launch.status === 'preparing'
        ? launch.label
        : null;
  const flowLabel = useSettledLabel(rawFlowLabel);

  if (inst && session) {
    const label = flowLabel || (session.stopping ? 'Stopping' : launchSessionActivityLabel(session));
    const tag = versionTag(inst.version_id);
    glyph = sessionGlyph(session);
    onClick = () => navigate({ name: 'instance', id: inst.id });
    title = `${label}: ${inst.name}`;
    ariaLabel = `Open active instance. ${label}: ${inst.name}`;
    content = (
      <>
        <span class="cp-status-pill-label">{label}</span>
        {tag && <Chip mono>{tag}</Chip>}
      </>
    );
  } else if (install) {
    const queueView = downloadQueue.value.view_model;
    const installPct = Math.round(Math.max(0, Math.min(100, install.pct)));
    const installName = install.displayName || install.item.versionId;
    glyph = 'downloading';
    progress = installPct;
    onClick = () => navigate({ name: 'downloads' });
    title = `${installName}: ${install.label}, ${installPct}%${queueView.active_queued_count_label || ''}`;
    ariaLabel = `Open downloads. ${title}`;
    content = (
      <>
        <span class="cp-status-pill-label">{installName}</span>
        <span class="cp-status-pill-pct">{installPct}%</span>
        {queueView.queued_count > 0 && <Chip>+{queueView.queued_count}</Chip>}
      </>
    );
  } else if (launch.status === 'preparing') {
    const li = instances.value.find((i) => i.id === launch.instanceId);
    const prepTag = versionTag(li?.version_id);
    glyph = 'preparing';
    title = `${launch.label}: ${li?.name || 'launch'}`;
    content = (
      <>
        <span class="cp-status-pill-label">{flowLabel || launch.label}</span>
        {prepTag && <Chip mono>{prepTag}</Chip>}
      </>
    );
  } else if (queueState.items.length > 0) {
    const queueView = queueState.view_model;
    glyph = 'queued';
    tone = undefined;
    onClick = () => navigate({ name: 'downloads' });
    title = `${queueView.queued_count_label}. ${queueView.next_label ? `Next: ${queueView.next_label}` : queueView.summary}`;
    ariaLabel = `Open downloads. ${title}`;
    content = (
      <>
        <span class="cp-status-pill-label">{queueView.status_label}</span>
        {queueView.queued_count > 1 && <Chip>{queueView.queued_count}</Chip>}
      </>
    );
  } else if (failure) {
    glyph = 'failed';
    tone = 'err';
    onClick = () => navigate({ name: 'downloads' });
    title = `${failure.displayName}: ${failure.viewModel.summary}`;
    ariaLabel = `Open downloads. ${failures.length} failed: ${title}`;
    content = <span class="cp-status-pill-label">Failed</span>;
  }

  const swapRef = useRef<HTMLSpanElement>(null);
  const [bodyWidth, setBodyWidth] = useState<number | null>(null);
  useLayoutEffect(() => {
    const el = swapRef.current;
    if (!el) return;
    const measure = (): void => setBodyWidth(el.getBoundingClientRect().width);
    measure();
    const ro = new ResizeObserver(() => measure());
    ro.observe(el);
    return () => ro.disconnect();
  }, [glyph]);

  if (!glyph) return null;
  return (
    <button
      class="cp-status-pill cp-nodrag"
      data-state={glyph}
      data-tone={tone}
      disabled={!onClick}
      onClick={onClick}
      title={title}
      aria-label={ariaLabel}
    >
      <StatusGlyph state={glyph} progress={progress} />
      <span class="cp-status-pill-body" style={bodyWidth != null ? { width: bodyWidth } : undefined}>
        <span class="cp-status-pill-swap" key={glyph} ref={swapRef}>
          {content}
        </span>
      </span>
    </button>
  );
}

function useWindowCalm(): boolean {
  const [calm, setCalm] = useState(false);
  useEffect(() => {
    const update = (): void => {
      setCalm(document.hidden || !document.hasFocus());
    };
    update();
    window.addEventListener('focus', update);
    window.addEventListener('blur', update);
    document.addEventListener('visibilitychange', update);
    return () => {
      window.removeEventListener('focus', update);
      window.removeEventListener('blur', update);
      document.removeEventListener('visibilitychange', update);
    };
  }, []);
  return calm;
}

export function Topbar(): JSX.Element {
  const [usesCustomDrag] = useState(hasCustomDragRegion());
  const calm = useWindowCalm();

  const onDragAreaDoubleClick = (e: MouseEvent): void => {
    if (!usesCustomDrag) return;
    if ((e.target as HTMLElement)?.closest('.cp-nodrag')) return;
    void windowToggleMaximize();
  };

  const onDragAreaMouseDown = (e: MouseEvent): void => {
    if (!usesCustomDrag) return;
    if ((e.target as HTMLElement)?.closest('.cp-nodrag')) return;
    if (e.button !== 0) return;
    void windowStartDragging();
  };

  const crumbs = crumbsFor();
  const sessionActive = Object.keys(launchSessions.value).length > 0;
  const flow = updateFlow.value;
  const hasUpdate = hasVisibleUpdate() || updateFlowActive();
  const updateBusy = flow.phase === 'downloading' || flow.phase === 'applying';
  const updateIndeterminate = flow.percent == null || flow.phase === 'applying';
  const updateRatio = flow.percent != null ? Math.min(100, Math.max(0, flow.percent)) / 100 : 0;
  return (
    <div
      class={`cp-topbar${usesCustomDrag ? ' cp-drag' : ''}`}
      data-calm={calm}
      onMouseDown={onDragAreaMouseDown}
      onDblClick={onDragAreaDoubleClick}
    >
      <div class="cp-topbar-nav cp-nodrag">
        <IconButton icon="arrow-left" size={28} tooltip="Back" onClick={goBack} />
        <IconButton icon="arrow-right" size={28} tooltip="Forward" onClick={goForward} />
      </div>
      <div class="cp-topbar-crumbs cp-nodrag">
        {crumbs.map((c, i) => (
          <>
            {i > 0 && <Icon name="chevron-right" size={12} />}
            <button class="cp-topbar-crumb" onClick={c.onClick} disabled={!c.onClick}>
              {c.label}
            </button>
          </>
        ))}
      </div>
      <button
        type="button"
        class="cp-topbar-search cp-nodrag"
        data-sound-silent="true"
        aria-label="Search and jump to"
        onClick={() => {
          commandPaletteOpen.value = true;
        }}
      >
        <Icon name="search" size={13} stroke={2} />
        <span>Search or jump to</span>
        <Kbd>{shortcutHint('command-palette')}</Kbd>
      </button>
      <div class="cp-topbar-spacer" />
      <div
        class="cp-topbar-actions cp-nodrag"
        style={
          updateBusy && !updateIndeterminate
            ? ({ '--cp-update-ratio': String(updateRatio) } as JSX.CSSProperties)
            : undefined
        }
      >
        <MusicWidget />
        <StatusPill />
        {hasUpdate && (
          <div class="cp-update-collapse" data-collapsed={sessionActive}>
            <UpdateWidget />
          </div>
        )}
      </div>
      <WindowControls />
    </div>
  );
}
