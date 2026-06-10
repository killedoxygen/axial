import type { JSX } from 'preact';
import { useMemo } from 'preact/hooks';
import { InstanceArt } from '../../art/InstanceArt';
import { Button, SectionHeading, Card, Pill } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { useTheme } from '../../hooks/use-theme';
import { navigate } from '../../ui-state';
import { config, instances, runningSessions, versions } from '../../store';
import { loaderKeyFromVersion, LOADER_LABELS } from '../create/defaults';
import type { EnrichedInstance, Version } from '../../types';

function greetingFor(date: Date): string {
  const h = date.getHours();
  if (h < 5) return 'Still up';
  if (h < 12) return 'Good morning';
  if (h < 18) return 'Good afternoon';
  return 'Good evening';
}

function formatDayDate(d: Date): string {
  const days = ['Sunday', 'Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday'];
  const months = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
  return `${days[d.getDay()]} · ${months[d.getMonth()]} ${d.getDate()}`;
}

function relativeTime(iso?: string): string {
  if (!iso) return 'never played';
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return 'never played';
  const diff = Date.now() - then;
  const m = Math.floor(diff / 60000);
  if (m < 1) return 'just now';
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  const d = Math.floor(h / 24);
  if (d < 7) return `${d}d ago`;
  const w = Math.floor(d / 7);
  if (w < 5) return `${w}w ago`;
  return new Date(iso).toLocaleDateString();
}

function versionBadge(v: Version | undefined): string {
  if (!v) return '—';
  return v.minecraft_meta.display_hint || v.minecraft_meta.display_name || v.id;
}

function loaderLabel(v: Version | undefined): string {
  return LOADER_LABELS[loaderKeyFromVersion(v)];
}

function ContinueStrip({ inst }: { inst: EnrichedInstance }): JSX.Element {
  const theme = useTheme();
  const version = versions.value.find(v => v.id === inst.version_id);
  const running = !!runningSessions.value[inst.id];
  const mods = inst.mods_count ?? 0;
  const openInstance = (): void => navigate({ name: 'instance', id: inst.id });
  const onKeyDown = (e: KeyboardEvent): void => {
    if (e.target !== e.currentTarget) return;
    if (e.key !== 'Enter' && e.key !== ' ') return;
    e.preventDefault();
    openInstance();
  };
  return (
    <div
      class="cp-card cp-continue"
      role="button"
      tabIndex={0}
      aria-label={`Open ${inst.name}`}
      onClick={openInstance}
      onKeyDown={onKeyDown}
    >
      <InstanceArt instance={inst} version={version} aspect="square" radius={theme.r.md} className="cp-continue-art" />
      <div class="cp-continue-body">
        <div class="cp-continue-kicker">{running ? 'Now playing' : 'Jump back in'}</div>
        <h2 title={inst.name}>{inst.name}</h2>
        <div class="cp-meta">
          <span>{loaderLabel(version)}</span>
          <span class="cp-dot" />
          <span>MC {versionBadge(version)}</span>
          <span class="cp-dot" />
          <span>{mods} mods</span>
          <span class="cp-dot" />
          <span>{relativeTime(inst.last_played_at)}</span>
        </div>
      </div>
      <div class="cp-continue-actions">
        {running && <Pill tone="accent" icon="play">Playing</Pill>}
        <Button
          size="lg"
          icon="play"
          title={`Play ${inst.name}`}
          onClick={(e) => { e.stopPropagation(); openInstance(); }}
          sound="launchPress"
        >Play</Button>
      </div>
    </div>
  );
}

const RECENT_COLS = '44px 2.4fr 1fr 1fr 1fr 84px';

function RecentRow({ inst }: { inst: EnrichedInstance }): JSX.Element {
  const theme = useTheme();
  const v = versions.value.find(x => x.id === inst.version_id);
  const running = !!runningSessions.value[inst.id];
  return (
    <div
      class="cp-table-row"
      style={{ gridTemplateColumns: RECENT_COLS }}
      onClick={() => navigate({ name: 'instance', id: inst.id })}
    >
      <InstanceArt instance={inst} aspect="thumb" radius={theme.r.sm} style={{ width: 32, height: 32 }} />
      <div style={{ minWidth: 0 }}>
        <div class="cp-table-row-title" style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
          {inst.name}
          {running && <Pill tone="accent" icon="play">Playing</Pill>}
        </div>
        <div class="cp-table-row-sub">{loaderLabel(v)}</div>
      </div>
      <div class="cp-table-cell">MC {versionBadge(v)}</div>
      <div class="cp-table-cell">{inst.mods_count ?? 0} mods</div>
      <div class="cp-table-cell">{relativeTime(inst.last_played_at)}</div>
      <div style={{ display: 'flex', justifyContent: 'flex-end' }}>
        <Button
          size="sm"
          variant="secondary"
          icon="play"
          onClick={(e) => { e.stopPropagation(); navigate({ name: 'instance', id: inst.id }); }}
        >Play</Button>
      </div>
    </div>
  );
}

function EmptyHome(): JSX.Element {
  return (
    <Card padding={32}>
      <div class="cp-empty">
        <Icon name="cube" size={36} color="var(--text-mute)" />
        <h2>Create your first instance</h2>
        <p>Instances are isolated Minecraft setups. Pick a version, bundle mods, and launch without touching your other worlds.</p>
        <Button icon="plus" onClick={() => navigate({ name: 'create' })}>New instance</Button>
      </div>
    </Card>
  );
}

export function HomeView(): JSX.Element {
  const cfg = config.value;
  const all = instances.value as EnrichedInstance[];
  const now = new Date();
  const recent = useMemo(() => {
    return [...all]
      .sort((a, b) => {
        const ta = a.last_played_at ? new Date(a.last_played_at).getTime() : 0;
        const tb = b.last_played_at ? new Date(b.last_played_at).getTime() : 0;
        return tb - ta;
      })
      .slice(0, 6);
  }, [all]);
  const totalMods = all.reduce((s, i) => s + (i.mods_count ?? 0), 0);
  const totalSaves = all.reduce((s, i) => s + (i.saves_count ?? 0), 0);
  const rest = recent.slice(1);

  return (
    <div class="cp-view-page">
      <div class="cp-page-header">
        <div>
          <h1>{greetingFor(now)}{cfg?.username ? `, ${cfg.username}` : ''}.</h1>
          <div class="cp-page-sub">
            {all.length === 0
              ? formatDayDate(now)
              : `${formatDayDate(now)} · ${all.length} instance${all.length === 1 ? '' : 's'} · ${totalMods} mods · ${totalSaves} saves`}
          </div>
        </div>
        <div style={{ flex: 1 }} />
        <Button variant="secondary" icon="plus" onClick={() => navigate({ name: 'create' })}>New instance</Button>
      </div>

      {all.length === 0 ? (
        <EmptyHome />
      ) : (
        <>
          <ContinueStrip inst={recent[0]} />
          {rest.length > 0 && (
            <div>
              <SectionHeading
                title="Recent"
                action={{ label: 'All instances', onClick: () => navigate({ name: 'instances' }) }}
              />
              <div class="cp-card cp-table">
                {rest.map(inst => <RecentRow key={inst.id} inst={inst} />)}
              </div>
            </div>
          )}
        </>
      )}
    </div>
  );
}
