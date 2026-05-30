import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
import { InstanceArt } from '../../art/InstanceArt';
import { Button, IconButton, Input, Segmented, Pill } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { openContextMenu } from '../../ui/ContextMenu';
import { useTheme } from '../../hooks/use-theme';
import { instances, versions, runningSessions } from '../../store';
import { navigate } from '../../ui-state';
import { createInstance } from '../../instance-create';
import { loaderKeyFromVersion, LOADER_LABELS } from '../create/defaults';
import { deleteInstanceFlow, duplicateInstance, openInstanceFolder, renameInstance } from '../instance/InstanceDetailView';
import type { EnrichedInstance, Version } from '../../types';
import { parseVersionDisplay } from '../../utils';

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
  return LOADER_LABELS[loaderKeyFromVersion(v)];
}

const LIST_COLS = '52px 2.4fr 1fr 1fr 1fr 140px';
const MIGRATION_VERSION_LIMIT = 4;

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
      <InstanceArt instance={inst} aspect="thumb" radius={theme.r.sm} style={{ width: 36, height: 36 }} />
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
  const openInstance = (): void => navigate({ name: 'instance', id: inst.id });
  const onCardKeyDown = (e: KeyboardEvent): void => {
    if (e.target !== e.currentTarget) return;
    if (e.key !== 'Enter' && e.key !== ' ') return;
    e.preventDefault();
    openInstance();
  };
  return (
    <div
      class="cp-card cp-playcard"
      role="button"
      tabIndex={0}
      aria-label={`Open ${inst.name}`}
      onClick={openInstance}
      onKeyDown={onCardKeyDown}
    >
      <InstanceArt instance={inst} version={v} aspect="square" radius={theme.r.md} style={{ width: 68, height: 68 }} />
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
    </div>
  );
}

function migrationVersionDisplay(version: Version): { title: string; detail: string } {
  const display = parseVersionDisplay(version.id, version, versions.value);
  const title = display.name === version.id ? versionLabel(version) : display.name;
  const detail = display.hint || version.minecraft_meta.effective_version || version.id;
  return { title, detail };
}

function migrationInstanceName(version: Version): string {
  const display = migrationVersionDisplay(version);
  return display.title.trim() || versionLabel(version);
}

function MigrationRow({
  version,
  busy,
  disabled,
  onCreate,
}: {
  version: Version;
  busy: boolean;
  disabled: boolean;
  onCreate: (version: Version) => void;
}): JSX.Element {
  const display = migrationVersionDisplay(version);
  const loader = loaderLabel(version);
  return (
    <div class="cp-migration-row">
      <div class="cp-migration-mark">
        <Icon name={version.loader ? 'puzzle' : 'cube'} size={16} color="var(--text-dim)" />
      </div>
      <div class="cp-migration-version">
        <strong>{display.title}</strong>
        <span>{loader} · {display.detail}</span>
      </div>
      <Button
        size="sm"
        variant="secondary"
        icon="plus"
        disabled={disabled}
        title={`Create instance from ${display.title}`}
        onClick={() => onCreate(version)}
      >
        {busy ? 'Creating' : 'Create'}
      </Button>
    </div>
  );
}

function InstalledVersionsEmpty({
  installedVersions,
}: {
  installedVersions: Version[];
}): JSX.Element {
  const [creatingVersionId, setCreatingVersionId] = useState<string | null>(null);
  const shown = installedVersions.slice(0, MIGRATION_VERSION_LIMIT);
  const remaining = installedVersions.length - shown.length;

  const createFromInstalled = (version: Version): void => {
    if (creatingVersionId) return;
    setCreatingVersionId(version.id);
    void createInstance({
      name: migrationInstanceName(version),
      versionId: version.id,
      icon: '',
      accent: '',
      install: { kind: 'none' },
    }).then((result) => {
      if (!result.ok) setCreatingVersionId(null);
    }, () => setCreatingVersionId(null));
  };

  return (
    <div class="cp-empty cp-migration-empty">
      <div class="cp-card cp-migration-panel">
        <div class="cp-migration-head">
          <Icon name="archive" size={34} color="var(--text-mute)" />
          <div>
            <h2>Installed versions found</h2>
            <p>Create an isolated instance from a version that is already installed.</p>
          </div>
        </div>
        <div class="cp-migration-list">
          {shown.map((version) => (
            <MigrationRow
              key={version.id}
              version={version}
              busy={creatingVersionId === version.id}
              disabled={creatingVersionId != null}
              onCreate={createFromInstalled}
            />
          ))}
        </div>
        <div class="cp-migration-foot">
          {remaining > 0 && <span>{remaining} more installed version{remaining === 1 ? '' : 's'} available</span>}
          <Button icon="plus" onClick={() => navigate({ name: 'create' })}>New instance</Button>
        </div>
      </div>
    </div>
  );
}

export function InstancesView(): JSX.Element {
  const theme = useTheme();
  const [view, setView] = useState<'list' | 'grid'>('list');
  const [q, setQ] = useState('');
  const all = instances.value as EnrichedInstance[];
  const query = q.trim().toLowerCase();
  const filtered = all.filter(i => i.name.toLowerCase().includes(query));
  const installedLaunchableVersions = [...versions.value]
    .filter(v => v.installed && v.launchable)
    .sort((a, b) => (b.release_time || '').localeCompare(a.release_time || ''));
  const showMigrationEmpty = all.length === 0 && query === '' && installedLaunchableVersions.length > 0;

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

      {showMigrationEmpty ? (
        <InstalledVersionsEmpty installedVersions={installedLaunchableVersions} />
      ) : filtered.length === 0 ? (
        <div class="cp-empty">
          <Icon name="cube" size={36} color="var(--text-mute)" />
          <h2>{q ? 'No matches' : 'No instances yet'}</h2>
          <p>{q ? 'Try a different search term.' : 'Create your first Minecraft instance to get started.'}</p>
          {!q && <Button icon="plus" onClick={() => navigate({ name: 'create' })}>New instance</Button>}
        </div>
      ) : view === 'list' ? (
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
      ) : (
        <div class="cp-grid-3">
          {filtered.map(i => <GridCard key={i.id} inst={i} />)}
        </div>
      )}
    </div>
  );
}
