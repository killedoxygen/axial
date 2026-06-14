import type { JSX } from 'preact';
import { useCallback, useState } from 'preact/hooks';
import { InstanceTile } from '../../ui/InstanceVisual';
import { Button, IconButton, Input, Segmented, Pill } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { InstanceCard } from '../../ui/InstanceCard';
import { openContextMenu } from '../../ui/ContextMenu';
import { SelectionActionPill, SelectionCheckbox } from '../../ui/SelectionActionPill';
import { selectionMenuItem, selectionToggleLabel, useSelection } from '../../ui/selection';
import { useTheme } from '../../hooks/use-theme';
import { instances, versionById, runningSessions } from '../../store';
import { instanceInstallStatus } from '../../instance-install-status';
import { navigate, openCreate } from '../../ui-state';
import { loaderKeyFromVersion, LOADER_LABELS } from '../create/defaults';
import { instanceMenuItems } from '../instance/instance-menu';
import { deleteInstancesFlow } from '../instance/instance-actions';
import { supportsMods } from '../../utils';
import { minecraftVersionLabel } from '../../version-display';
import { fmtRelativeCompact } from '../instance/format';
import type { EnrichedInstance, Version } from '../../types';

function versionLabel(v: Version | undefined): string {
  return minecraftVersionLabel(v, 'Unknown');
}

function loaderLabel(v: Version | undefined): string {
  return LOADER_LABELS[loaderKeyFromVersion(v)];
}

const LIST_COLS = '28px 52px 2.4fr 1fr 1fr 1fr 140px';

function ListRow({
  inst,
  selected,
  onToggleSelect,
  onContextMenu,
}: {
  inst: EnrichedInstance;
  selected: boolean;
  onToggleSelect: (e: MouseEvent) => void;
  onContextMenu: (e: MouseEvent) => void;
}): JSX.Element {
  const theme = useTheme();
  const v = versionById(inst.version_id);
  const running = !!runningSessions.value[inst.id];
  const install = instanceInstallStatus(inst, v);
  const installing = install.installing;
  const installLabel = install.state === 'queued' ? 'Queued' : 'Installing';
  const showModsCount = supportsMods(v);
  return (
    <div
      class="cp-table-row cp-selection-row"
      style={{ gridTemplateColumns: LIST_COLS }}
      data-selected={selected}
      onClick={() => navigate({ name: 'instance', id: inst.id })}
      onContextMenu={onContextMenu}
    >
      <SelectionCheckbox
        selected={selected}
        label={selectionToggleLabel(selected, inst.name)}
        onToggle={(e) => {
          e.stopPropagation();
          onToggleSelect(e);
        }}
      />
      <InstanceTile inst={inst} radius={theme.r.sm} style={{ width: 36, height: 36 }} />
      <div>
        <div class="cp-table-row-title" style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
          {inst.name}
          {running && (
            <Pill tone="accent" icon="play">
              Live
            </Pill>
          )}
          {installing && <Pill icon={install.state === 'queued' ? 'clock' : 'download'}>{installLabel}</Pill>}
        </div>
        <div class="cp-table-row-sub">
          {loaderLabel(v)} · {v?.loader?.loader_version || 'vanilla'}
        </div>
      </div>
      <div class="cp-table-cell">{versionLabel(v)}</div>
      <div class="cp-table-cell">{showModsCount ? `${inst.mods_count ?? 0} mods` : null}</div>
      <div class="cp-table-cell">{fmtRelativeCompact(inst.last_played_at)}</div>
      <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 4 }}>
        <Button
          size="sm"
          variant="secondary"
          icon={installing ? (install.state === 'queued' ? 'clock' : 'download') : 'play'}
          disabled={installing}
          onClick={(e) => {
            e.stopPropagation();
            navigate({ name: 'instance', id: inst.id });
          }}
        >
          {installing ? installLabel : 'Play'}
        </Button>
        <IconButton
          icon="dots"
          size={28}
          onClick={(e: any) => {
            e.stopPropagation();
            onContextMenu(e);
          }}
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
  const filtered = all.filter((i) => i.name.toLowerCase().includes(query));
  const selection = useSelection(
    filtered,
    useCallback((inst: EnrichedInstance) => inst.id, []),
  );

  const menuItems = (inst: EnrichedInstance) => [
    selectionMenuItem(selection, inst.id),
    { divider: true, label: '', onSelect: () => undefined },
    ...instanceMenuItems(inst),
  ];

  const openMenu = (e: MouseEvent, inst: EnrichedInstance): void => {
    openContextMenu(e, menuItems(inst));
  };

  const deleteSelected = async (): Promise<void> => {
    await deleteInstancesFlow(selection.selectedItems, selection.clear);
  };

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
        <Segmented<'grid' | 'list'>
          value={view}
          onChange={setView}
          options={[
            { value: 'grid', label: 'Grid' },
            { value: 'list', label: 'List' },
          ]}
        />
        <Button icon="plus" onClick={openCreate}>
          New
        </Button>
      </div>

      {filtered.length === 0 ? (
        <div class="cp-empty">
          <Icon name="stack" size={36} color="var(--text-mute)" />
          <h2>{q ? 'No matches' : 'No instances yet'}</h2>
          <p>{q ? 'Try a different search term.' : 'Create your first Minecraft instance to get started.'}</p>
          {!q && (
            <Button icon="plus" onClick={openCreate}>
              New instance
            </Button>
          )}
        </div>
      ) : view === 'grid' ? (
        <div class="cp-cover-grid">
          {filtered.map((i) => (
            <InstanceCard
              key={i.id}
              inst={i}
              selected={selection.isSelected(i.id)}
              onToggleSelect={() => selection.toggle(i.id)}
              onContextMenu={(e) => openMenu(e, i)}
            />
          ))}
        </div>
      ) : (
        <div class="cp-card cp-table">
          <div class="cp-table-head" style={{ gridTemplateColumns: LIST_COLS }}>
            <span />
            <span />
            <span>Instance</span>
            <span>Version</span>
            <span>Mods</span>
            <span>Last played</span>
            <span />
          </div>
          {filtered.map((i) => (
            <ListRow
              key={i.id}
              inst={i}
              selected={selection.isSelected(i.id)}
              onToggleSelect={() => selection.toggle(i.id)}
              onContextMenu={(e) => openMenu(e, i)}
            />
          ))}
        </div>
      )}
      <SelectionActionPill
        selection={selection}
        itemLabel="instance"
        actions={[{ label: 'Delete', icon: 'trash', danger: true, onClick: () => void deleteSelected() }]}
      />
    </div>
  );
}
