import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
import { Thumb } from '../../ui/Thumb';
import { Button, IconButton, Input, Segmented, Pill } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { openContextMenu } from '../../ui/ContextMenu';
import { useTheme } from '../../hooks/use-theme';
import { instances, versions, runningSessions } from '../../store';
import { navigate } from '../../ui-state';
import { deleteInstanceFlow, duplicateInstance, openInstanceFolder, renameInstance } from '../instance/InstanceDetailView';
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
  if (!v) return '—';
  return v.minecraft_meta.display_hint || v.minecraft_meta.display_name || v.id;
}

function loaderLabel(v: Version | undefined): string {
  if (!v?.loader) return 'Vanilla';
  const id = v.loader.component_id;
  if (id.includes('fabric')) return 'Fabric';
  if (id.includes('quilt')) return 'Quilt';
  if (id.includes('neoforged')) return 'NeoForge';
  if (id.includes('minecraftforge')) return 'Forge';
  return 'Modded';
}

const LIST_COLS = '52px 2.4fr 1fr 1fr 1fr 140px';

function menuItemsFor(inst: EnrichedInstance): Parameters<typeof openContextMenu>[1] {
  return [
    { icon: 'play', label: 'Open detail', onSelect: () => navigate({ name: 'instance', id: inst.id }) },
    { icon: 'folder', label: 'Open folder', onSelect: () => void openInstanceFolder(inst.id) },
    { icon: 'copy', label: 'Duplicate', onSelect: () => void duplicateInstance(inst) },
    { icon: 'edit', label: 'Rename', onSelect: () => void renameInstance(inst) },
    { label: '', onSelect: () => {}, divider: true },
    { icon: 'trash', label: 'Delete', onSelect: () => void deleteInstanceFlow(inst), danger: true },
  ];
}

function ListRow({ inst }: { inst: EnrichedInstance }): JSX.Element {
  const theme = useTheme();
  const v = versions.value.find(x => x.id === inst.version_id);
  const running = !!runningSessions.value[inst.id];
  return (
    <div
      class="cp-table-row"
      style={{ gridTemplateColumns: LIST_COLS }}
      onClick={() => navigate({ name: 'instance', id: inst.id })}
      onContextMenu={(e) => openContextMenu(e, menuItemsFor(inst))}
    >
      <Thumb name={inst.name} size={36} radius={theme.r.sm} />
      <div>
        <div class="cp-table-row-title" style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
          {inst.name}
          {running && <Pill tone="accent" icon="play">Live</Pill>}
        </div>
        <div class="cp-table-row-sub">{loaderLabel(v)} · {v?.loader?.loader_version || 'vanilla'}</div>
      </div>
      <div style={{ fontSize: 12, color: theme.n.textDim }}>{versionLabel(v)}</div>
      <div style={{ fontSize: 12, color: theme.n.textDim }}>{inst.mods_count ?? 0} mods</div>
      <div style={{ fontSize: 12, color: theme.n.textDim }}>{fmtRelative(inst.last_played_at)}</div>
      <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 4 }}>
        <Button
          size="sm"
          icon="play"
          onClick={(e) => { e.stopPropagation(); navigate({ name: 'instance', id: inst.id }); }}
        >Play</Button>
        <IconButton
          icon="dots"
          size={28}
          onClick={(e: any) => { e.stopPropagation(); openContextMenu(e, menuItemsFor(inst)); }}
        />
      </div>
    </div>
  );
}

function GridCard({ inst }: { inst: EnrichedInstance }): JSX.Element {
  const theme = useTheme();
  const v = versions.value.find(x => x.id === inst.version_id);
  return (
    <button
      class="cp-playcard"
      style={{ border: 'none', font: 'inherit', color: 'inherit' }}
      onClick={() => navigate({ name: 'instance', id: inst.id })}
    >
      <Thumb name={inst.name} size={68} radius={theme.r.md} />
      <div class="cp-playcard-body">
        <div class="cp-playcard-title">
          <h3>{inst.name}</h3>
        </div>
        <div class="cp-playcard-meta">
          <span>{loaderLabel(v)}</span>
          <span class="cp-dot" />
          <span>MC {versionLabel(v)}</span>
          <span class="cp-dot" />
          <span>{inst.mods_count ?? 0} mods</span>
        </div>
      </div>
      <Button size="sm" icon="play" onClick={(e) => { e.stopPropagation(); navigate({ name: 'instance', id: inst.id }); }}>Play</Button>
    </button>
  );
}

export function InstancesView(): JSX.Element {
  const theme = useTheme();
  const [view, setView] = useState<'list' | 'grid'>('list');
  const [q, setQ] = useState('');
  const all = instances.value as EnrichedInstance[];
  const filtered = all.filter(i => i.name.toLowerCase().includes(q.toLowerCase()));

  return (
    <div class="cp-view-page" style={{ gap: 16 }}>
      <div class="cp-page-header">
        <div>
          <h1>Instances</h1>
          <div class="cp-page-sub">
            {all.length} total · {all.reduce((s, i) => s + (i.mods_count ?? 0), 0)} mods across all
          </div>
        </div>
        <div style={{ flex: 1 }} />
        <Input value={q} onChange={setQ} placeholder="Filter instances…" icon="search" style={{ width: 260 }} />
        <Segmented<'list' | 'grid'> value={view} onChange={setView}
          options={[{ value: 'list', label: 'List' }, { value: 'grid', label: 'Grid' }]} />
        <Button icon="plus" onClick={() => navigate({ name: 'create' })}>New</Button>
      </div>

      {filtered.length === 0 ? (
        <div class="cp-empty">
          <Icon name="cube" size={36} color="var(--text-mute)" />
          <h2>{q ? 'No matches' : 'No instances yet'}</h2>
          <p>{q ? 'Try a different search term.' : 'Create your first Minecraft instance to get started.'}</p>
          {!q && <Button icon="plus" onClick={() => navigate({ name: 'create' })}>New instance</Button>}
        </div>
      ) : view === 'list' ? (
        <div class="cp-table">
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
      ) : (
        <div class="cp-grid-3">
          {filtered.map(i => <GridCard key={i.id} inst={i} />)}
        </div>
      )}
    </div>
  );
}
