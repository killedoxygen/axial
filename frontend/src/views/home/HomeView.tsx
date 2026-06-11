import type { JSX } from 'preact';
import { useMemo } from 'preact/hooks';
import { InstanceArt } from '../../art/InstanceArt';
import { Button, SectionHeading, Card, Pill } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { InstanceCard } from '../../ui/InstanceCard';
import { navigate, openCreate } from '../../ui-state';
import { config, instances, runningSessions, versions } from '../../store';
import { loaderKeyFromVersion, LOADER_LABELS } from '../create/defaults';
import { openInstanceContextMenu } from '../instance/instance-menu';
import { supportsMods } from '../../utils';
import type { EnrichedInstance, Version } from '../../types';

function greetingFor(date: Date): string {
  const h = date.getHours();
  if (h < 5) return 'Still up';
  if (h < 12) return 'Good morning';
  if (h < 18) return 'Good afternoon';
  return 'Good evening';
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

function FeatureBanner({ inst }: { inst: EnrichedInstance }): JSX.Element {
  const version = versions.value.find(v => v.id === inst.version_id);
  const running = !!runningSessions.value[inst.id];
  const mods = inst.mods_count ?? 0;
  const showModsCount = supportsMods(version);
  const open = (): void => navigate({ name: 'instance', id: inst.id });
  const onKeyDown = (e: KeyboardEvent): void => {
    if (e.target !== e.currentTarget) return;
    if (e.key !== 'Enter' && e.key !== ' ') return;
    e.preventDefault();
    open();
  };
  return (
    <div
      class="cp-feature"
      role="button"
      tabIndex={0}
      aria-label={`Open ${inst.name}`}
      onClick={open}
      onKeyDown={onKeyDown}
      onContextMenu={(e) => openInstanceContextMenu(e, inst)}
    >
      <InstanceArt instance={inst} version={version} aspect="banner" radius={0} className="cp-feature-art" />
      <div class="cp-feature-scrim" />
      <div class="cp-feature-content">
        <div class="cp-feature-id">
          <div class="cp-feature-kicker">{running ? 'Now playing' : 'Jump back in'}</div>
          <h2 title={inst.name}>{inst.name}</h2>
          <div class="cp-meta">
            <span>{LOADER_LABELS[loaderKeyFromVersion(version)]}</span>
            <span class="cp-dot" />
            <span>MC {versionBadge(version)}</span>
            {showModsCount && (
              <>
                <span class="cp-dot" />
                <span>{mods} mods</span>
              </>
            )}
            <span class="cp-dot" />
            <span>{relativeTime(inst.last_played_at)}</span>
          </div>
        </div>
        <div class="cp-feature-actions">
          {running && <Pill tone="accent" icon="play">Playing</Pill>}
          <Button
            size="lg"
            icon="play"
            title={`Play ${inst.name}`}
            onClick={(e) => { e.stopPropagation(); open(); }}
            sound="launchPress"
          >Play</Button>
        </div>
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
        <Button icon="plus" onClick={openCreate}>New instance</Button>
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
      .slice(0, 13);
  }, [all]);
  const rest = recent.slice(1);

  return (
    <div class="cp-view-page">
      <div class="cp-page-header">
        <div>
          <h1>{greetingFor(now)}{cfg?.username ? `, ${cfg.username}` : ''}.</h1>
          <div class="cp-page-sub">
            {all.length === 0
              ? 'Set up your first instance to start playing.'
              : `${all.length} instance${all.length === 1 ? '' : 's'} in your library`}
          </div>
        </div>
        <div style={{ flex: 1 }} />
        <Button variant="secondary" icon="plus" onClick={openCreate}>New instance</Button>
      </div>

      {all.length === 0 ? (
        <EmptyHome />
      ) : (
        <>
          <FeatureBanner inst={recent[0]} />
          {rest.length > 0 && (
            <div>
              <SectionHeading
                title="Library"
                action={{ label: 'See all', onClick: () => navigate({ name: 'instances' }) }}
              />
              <div class="cp-cover-grid">
                {rest.map(inst => (
                  <InstanceCard
                    key={inst.id}
                    inst={inst}
                    onContextMenu={(e) => openInstanceContextMenu(e, inst)}
                  />
                ))}
              </div>
            </div>
          )}
        </>
      )}
    </div>
  );
}
