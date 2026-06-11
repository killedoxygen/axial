import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
import { InstanceTile } from '../../ui/InstanceVisual';
import { Button, IconButton, Input, Segmented, Pill } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { InstanceCard } from '../../ui/InstanceCard';
import { useTheme } from '../../hooks/use-theme';
import { instances, versions, runningSessions } from '../../store';
import { navigate, openCreate } from '../../ui-state';
import { loaderKeyFromVersion, LOADER_LABELS } from '../create/defaults';
import { openInstanceContextMenu } from '../instance/instance-menu';
import { supportsMods } from '../../utils';
import { minecraftVersionLabel } from '../../version-display';
import type { EnrichedInstance, Version } from '../../types';

function fmtRelative(iso?: string): string {
  if (!iso) return 'never';
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return 'never';
  const diff = Date.now() - then;
  const m = Math.floor(diff / 60000);
  if (m < 1) return 'just now';
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h`;
  const d = Math.floor(h / 24);
  if (d < 30) return `${d}d`;
  const mo = Math.floor(d / 30);
  return `${mo}mo`;
}

function versionLabel(v: Version | undefined): string {
  return minecraftVersionLabel(v, '—');
}

function loaderLabel(v: Version | undefined): string {
  return LOADER_LABELS[loaderKeyFromVersion(v)];
}

const LIST_COLS = '52px 2.4fr 1fr 1fr 1fr 140px';

function ListRow({ inst }: { inst: EnrichedInstance }): JSX.Element {
  const theme = useTheme();
  const v = versions.value.find(x => x.id === inst.version_id);
  const running = !!runningSessions.value[inst.id];
  const showModsCount = supportsMods(v);
  return (
    <div
      class="cp-table-row"
      style={{ gridTemplateColumns: LIST_COLS }}
      onClick={() => navigate({ name: 'instance', id: inst.id })}
      onContextMenu={(e) => openInstanceContextMenu(e, inst)}
    >
      <InstanceTile inst={inst} radius={theme.r.sm} style={{ width: 36, height: 36 }} />
      <div>
        <div class="cp-table-row-title" style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
          {inst.name}
          {running && <Pill tone="accent" icon="play">Live</Pill>}
        </div>
        <div class="cp-table-row-sub">{loaderLabel(v)} · {v?.loader?.loader_version || 'vanilla'}</div>
      </div>
      <div class="cp-table-cell">{versionLabel(v)}</div>
      <div class="cp-table-cell">{showModsCount ? `${inst.mods_count ?? 0} mods` : null}</div>
      <div class="cp-table-cell">{fmtRelative(inst.last_played_at)}</div>
      <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 4 }}>
        <Button
          size="sm"
          variant="secondary"
          icon="play"
          onClick={(e) => { e.stopPropagation(); navigate({ name: 'instance', id: inst.id }); }}
        >Play</Button>
        <IconButton
          icon="dots"
          size={28}
          onClick={(e: any) => { e.stopPropagation(); openInstanceContextMenu(e, inst); }}
        />
      </div>
    </div>
  );
}

export function InstancesView(): JSX.Element {
  const [view, setView] = useState<'grid' | 'list'>('grid');
  const [q, setQ] = useState('');
  const all = instances.value as EnrichedInstance[];
  const query = q.trim().toLowerCase();
  const filtered = all.filter(i => i.name.toLowerCase().includes(query));

  return (
    <div class="cp-view-page" style={{ gap: 18 }}>
      <div class="cp-page-header">
        <div>
          <h1>Instances</h1>
          <div class="cp-page-sub">
            {all.length} total · {all.reduce((s, i) => s + (i.mods_count ?? 0), 0)} mods across all
          </div>
        </div>
        <div style={{ flex: 1 }} />
        <Input value={q} onChange={setQ} placeholder="Filter instances…" icon="search" style={{ width: 260 }} />
        <Segmented<'grid' | 'list'> value={view} onChange={setView}
          options={[{ value: 'grid', label: 'Grid' }, { value: 'list', label: 'List' }]} />
        <Button icon="plus" onClick={openCreate}>New</Button>
      </div>

      {filtered.length === 0 ? (
        <div class="cp-empty">
          <Icon name="cube" size={36} color="var(--text-mute)" />
          <h2>{q ? 'No matches' : 'No instances yet'}</h2>
          <p>{q ? 'Try a different search term.' : 'Create your first Minecraft instance to get started.'}</p>
          {!q && <Button icon="plus" onClick={openCreate}>New instance</Button>}
        </div>
      ) : view === 'grid' ? (
        <div class="cp-cover-grid">
          {filtered.map(i => (
            <InstanceCard
              key={i.id}
              inst={i}
              onContextMenu={(e) => openInstanceContextMenu(e, i)}
            />
          ))}
        </div>
      ) : (
        <div class="cp-card cp-table">
          <div class="cp-table-head" style={{ gridTemplateColumns: LIST_COLS }}>
            <span />
            <span>Instance</span>
            <span>Version</span>
            <span>Mods</span>
            <span>Last played</span>
            <span />
          </div>
          {filtered.map(i => <ListRow key={i.id} inst={i} />)}
        </div>
      )}
    </div>
  );
}
