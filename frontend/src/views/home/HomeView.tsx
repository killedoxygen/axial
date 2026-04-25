import type { JSX } from 'preact';
import { useMemo } from 'preact/hooks';
import { InstanceArt } from '../../art/InstanceArt';
import { Button, SectionHeading, Card, Pill } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { useTheme } from '../../hooks/use-theme';
import { navigate } from '../../ui-state';
import { config, instances, runningSessions, versions } from '../../store';
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
  if (!v?.loader) return 'Vanilla';
  const componentId = v.loader.component_id;
  if (componentId.includes('fabric')) return 'Fabric';
  if (componentId.includes('quilt')) return 'Quilt';
  if (componentId.includes('neoforged')) return 'NeoForge';
  if (componentId.includes('minecraftforge')) return 'Forge';
  return 'Mods';
}

function PlayCard({ inst }: { inst: EnrichedInstance }): JSX.Element {
  const theme = useTheme();
  const version = versions.value.find(v => v.id === inst.version_id);
  const running = runningSessions.value[inst.id];
  const mods = inst.mods_count ?? 0;
  return (
    <button
      class="cp-playcard"
      style={{ border: 'none', font: 'inherit', color: 'inherit', cursor: 'pointer', background: undefined }}
      onClick={() => navigate({ name: 'instance', id: inst.id })}
    >
      <InstanceArt instance={inst} aspect="square" radius={theme.r.md} style={{ width: 68, height: 68 }} />
      <div class="cp-playcard-body">
        <div class="cp-playcard-title">
          <h3>{inst.name}</h3>
          {running && <Pill tone="accent" icon="play">Playing</Pill>}
        </div>
        <div class="cp-playcard-meta">
          <span>{loaderLabel(version)}</span>
          <span class="cp-dot" />
          <span>MC {versionBadge(version)}</span>
          <span class="cp-dot" />
          <span>{mods} mods</span>
          <span class="cp-dot" />
          <span>{relativeTime(inst.last_played_at)}</span>
        </div>
      </div>
      <Button
        size="md"
        icon="play"
        onClick={(e) => { e.stopPropagation(); navigate({ name: 'instance', id: inst.id }); }}
      >Play</Button>
    </button>
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
  const theme = useTheme();
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
      .slice(0, 4);
  }, [all]);
  const totalMods = all.reduce((s, i) => s + (i.mods_count ?? 0), 0);
  const totalSaves = all.reduce((s, i) => s + (i.saves_count ?? 0), 0);

  return (
    <div class="cp-view-page">
      <div class="cp-hero">
        <div>
          <div class="cp-hero-eyebrow">{formatDayDate(now)}</div>
          <h1>{greetingFor(now)}{cfg?.username ? `, ${cfg.username}` : ''}.</h1>
          <div class="cp-hero-sub">
            {all.length === 0
              ? 'Nothing installed yet, spin up your first instance'
              : `${all.length} instance${all.length === 1 ? '' : 's'} · ${totalMods} mods · ${totalSaves} saves`}
          </div>
        </div>
        <div class="cp-hero-actions">
          <Button variant="secondary" icon="plus" onClick={() => navigate({ name: 'create' })}>New instance</Button>
          {recent[0] && (
            <Button icon="play" onClick={() => navigate({ name: 'instance', id: recent[0].id })}>
              Resume {recent[0].name}
            </Button>
          )}
        </div>
      </div>

      {all.length === 0 ? (
        <EmptyHome />
      ) : (
        <div>
          <SectionHeading
            eyebrow="Continue"
            title="Recent instances"
            action={{ label: 'All instances', onClick: () => navigate({ name: 'instances' }) }}
          />
          <div class="cp-grid-2">
            {recent.map(inst => <PlayCard key={inst.id} inst={inst} />)}
          </div>
        </div>
      )}

      <div>
        <SectionHeading eyebrow="Library" title="At a glance" />
        <div class="cp-grid-3">
          <Card>
            <div style={{ fontSize: 11, fontWeight: 600, color: theme.n.textMute, letterSpacing: 0.6, textTransform: 'uppercase' }}>Instances</div>
            <div style={{ fontSize: 32, fontWeight: 600, marginTop: 6, letterSpacing: -0.6 }}>{all.length}</div>
          </Card>
          <Card>
            <div style={{ fontSize: 11, fontWeight: 600, color: theme.n.textMute, letterSpacing: 0.6, textTransform: 'uppercase' }}>Mods installed</div>
            <div style={{ fontSize: 32, fontWeight: 600, marginTop: 6, letterSpacing: -0.6 }}>{totalMods}</div>
          </Card>
          <Card>
            <div style={{ fontSize: 11, fontWeight: 600, color: theme.n.textMute, letterSpacing: 0.6, textTransform: 'uppercase' }}>World saves</div>
            <div style={{ fontSize: 32, fontWeight: 600, marginTop: 6, letterSpacing: -0.6 }}>{totalSaves}</div>
          </Card>
        </div>
      </div>
    </div>
  );
}
