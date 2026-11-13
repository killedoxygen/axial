import type { JSX } from 'preact';
import { InstanceFacts, SessionChip } from '../../ui/InstanceFacts';
import { useLayoutEffect, useMemo, useRef, useState } from 'preact/hooks';
import { Button, Chip } from '../../ui/Atoms';
import { Face } from '../../ui/Face';
import { Icon } from '../../ui/Icons';
import { InstanceTile, artSeedFor } from '../../ui/InstanceVisual';
import { Vista } from '../../vista/Vista';
import { selectInstance } from '../../actions';
import { fmtRelative } from '../../format';
import { launchGame } from '../../launch';
import { navigate, openCreate } from '../../ui-state';
import { config, instances, launchSessions, launchState, versionById } from '../../store';
import { instanceInstallStatus } from '../../instance-install-status';
import { openInstanceContextMenu } from '../instance/instance-menu';
import type { EnrichedInstance } from '../../types-instance';

const GAP = 28;
const SHELF_LIMIT = 6;

function greetingFor(date: Date): string {
  const h = date.getHours();
  if (h < 5) return 'Still up';
  if (h < 12) return 'Good morning';
  if (h < 18) return 'Good afternoon';
  return 'Good evening';
}

/** The last-played instance: its name as display type, its facts, and the way in. */
function Lead({ inst }: { inst: EnrichedInstance }): JSX.Element {
  const session = launchSessions.value[inst.id];
  const install = instanceInstallStatus(inst, versionById(inst.version_id));
  const action = inst.launch_action;
  const open = (): void => navigate({ name: 'instance', id: inst.id });
  const canPlay = action.launchable && !session && !install.installing;
  // Just after an install the instance is still as it was before it; it stays busy until it is re-read.
  const busy = install.installing || (install.settling && !action.launchable);
  const play = (): void => {
    selectInstance(inst.id);
    void launchGame();
  };
  return (
    <section class="cp-home-lead" aria-label={inst.name} onContextMenu={(e) => openInstanceContextMenu(e, inst)}>
      <h2 class="cp-home-name" title={inst.name}>
        {inst.name}
      </h2>
      <div class="cp-chips">
        <InstanceFacts inst={inst} />
        {session ? (
          <SessionChip session={session} />
        ) : install.installing ? (
          <Chip tone="accent" icon={install.state === 'queued' ? 'clock' : 'download'}>
            {install.state === 'queued' ? install.queuedItem?.title || install.label : 'Installing'}
          </Chip>
        ) : (
          <Chip icon="clock">{inst.last_played_at ? fmtRelative(inst.last_played_at) : 'Not played yet'}</Chip>
        )}
      </div>
      <div class="cp-home-actions">
        {canPlay ? (
          <>
            <Button
              size="lg"
              icon="play"
              sound="launchPress"
              busy={launchState.value.status === 'preparing'}
              onClick={play}
            >
              Play
            </Button>
            <Button size="lg" variant="secondary" onClick={open}>
              Open
            </Button>
          </>
        ) : (
          <Button
            size="lg"
            icon={
              session || busy ? undefined : action.primary_action === 'install' ? 'download' : 'alert'
            }
            title={action.primary_action === 'blocked' ? action.disabled_reason : undefined}
            onClick={open}
          >
            {session || busy ? 'Open' : action.label}
          </Button>
        )}
      </div>
    </section>
  );
}

/** One shelf of covers, as many as fit, then the way to add one. */
function Shelf({ list, total }: { list: EnrichedInstance[]; total: number }): JSX.Element {
  const rowRef = useRef<HTMLDivElement>(null);
  const [fit, setFit] = useState(SHELF_LIMIT);
  useLayoutEffect(() => {
    const row = rowRef.current;
    if (!row) return undefined;
    const measure = (): void =>
      setFit(
        Math.max(
          1,
          Math.min(SHELF_LIMIT, Math.floor((row.clientWidth + GAP) / (row.lastElementChild!.clientWidth + GAP)) - 1),
        ),
      );
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(row);
    return () => observer.disconnect();
  }, []);
  const shown = list.slice(0, fit);
  return (
    <section class="cp-home-shelf">
      <div class="cp-head">
        <h2>Library</h2>
        <Button
          variant="ghost"
          size="sm"
          trailing={<Icon name="chevron-right" size={13} />}
          onClick={() => navigate({ name: 'instances' })}
        >
          {total > shown.length + 1 ? `See all ${total}` : 'See all'}
        </Button>
      </div>
      <div class="cp-home-row" ref={rowRef}>
        {shown.map((inst) => (
          <button
            key={inst.id}
            type="button"
            class="cp-home-item"
            aria-label={`Open ${inst.name}`}
            onClick={() => navigate({ name: 'instance', id: inst.id })}
            onContextMenu={(e) => openInstanceContextMenu(e, inst)}
          >
            <span class="cp-home-cover">
              <InstanceTile inst={inst} />
              <Vista
                seed={artSeedFor(inst)}
                version={inst.version_id}
                installing={instanceInstallStatus(inst, versionById(inst.version_id)).installing}
              />
            </span>
            <span class="cp-home-title" title={inst.name}>
              {inst.name}
            </span>
          </button>
        ))}
        <button type="button" class="cp-home-item cp-home-new" onClick={openCreate}>
          <span class="cp-home-cover">
            <Icon name="plus" size={22} />
          </span>
          <span class="cp-home-title">New instance</span>
        </button>
      </div>
    </section>
  );
}

export function HomeView(): JSX.Element {
  const all = instances.value;
  const username = config.value?.username;
  const recent = useMemo(
    () =>
      [...all].sort((a, b) => {
        const ta = a.last_played_at ? new Date(a.last_played_at).getTime() : 0;
        const tb = b.last_played_at ? new Date(b.last_played_at).getTime() : 0;
        return tb - ta;
      }),
    [all],
  );
  const lead = recent[0];

  if (!lead) {
    return (
      <div class="cp-view-page cp-home cp-home--empty">
        <Face size={96} />
        <h1>No instances yet</h1>
        <Button size="lg" icon="plus" onClick={openCreate}>
          New instance
        </Button>
      </div>
    );
  }
  return (
    <div class="cp-view-page cp-home">
      <Vista
        seed={artSeedFor(lead)}
        version={lead.version_id}
        installing={instanceInstallStatus(lead, versionById(lead.version_id)).installing}
      />
      <h1 class="cp-home-hello">
        {greetingFor(new Date())}
        {username ? `, ${username}` : ''}.
      </h1>
      <Lead inst={lead} />
      <Shelf list={recent.slice(1)} total={all.length} />
    </div>
  );
}
