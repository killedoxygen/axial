import type { JSX } from 'preact';
import { SessionChip } from '../../ui/InstanceFacts';
import { useCallback, useState } from 'preact/hooks';
import { InstanceTile } from '../../ui/InstanceVisual';
import { Button, Empty, IconButton, Input } from '../../ui/Atoms';
import { Segmented } from '../../ui/Segmented';
import { Icon } from '../../ui/Icons';
import { InstanceCard, openOnKey } from '../../ui/InstanceCard';
import { openContextMenu } from '../../ui/ContextMenu';
import { SelectionActionTray, SelectionCheckbox } from '../../ui/SelectionActionTray';
import { selectionMenuItem, selectionToggleLabel, useSelection } from '../../ui/selection';
import { instances, versionById, launchSessions } from '../../store';
import { instanceInstallStatus } from '../../instance-install-status';
import { navigate, openCreate } from '../../ui-state';
import { instanceMenuItems } from '../instance/instance-menu';
import { deleteInstancesFlow } from '../instance/instance-actions';
import { fmtRelative } from '../../format';
import type { EnrichedInstance } from '../../types-instance';
import { PendingRemovalsNotice } from './PendingRemovalsNotice';

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
  const session = launchSessions.value[inst.id];
  const install = instanceInstallStatus(inst, versionById(inst.version_id));
  const installing = install.installing;
  const installLabel = install.state === 'queued' ? install.queuedItem?.title || install.label : 'Installing';
  const launchAction = inst.launch_action;
  const actionIcon =
    launchAction.primary_action === 'launch'
      ? 'play'
      : launchAction.primary_action === 'install'
        ? 'download'
        : 'alert';
  const open = (): void => navigate({ name: 'instance', id: inst.id });
  return (
    <div
      class="cp-tr"
      role="button"
      tabIndex={0}
      aria-label={installing ? `Open ${inst.name}. ${installLabel}` : `Open ${inst.name}`}
      data-selected={selected}
      onClick={open}
      onKeyDown={(e) => openOnKey(e, open)}
      onContextMenu={onContextMenu}
    >
      <span class="cp-library-name">
        <span class="cp-lead">
          <InstanceTile inst={inst} />
          <SelectionCheckbox
            selected={selected}
            label={selectionToggleLabel(selected, inst.name)}
            onToggle={(e) => {
              e.stopPropagation();
              onToggleSelect(e);
            }}
          />
        </span>
        <strong>{inst.name}</strong>
        {session && (
          <SessionChip session={session} />
        )}
      </span>
      <span class="cp-library-cell">
        {inst.version_display.loader_label}
        {inst.version_display.loader_version_label ? ` ${inst.version_display.loader_version_label}` : ''}
      </span>
      <span class="cp-library-num">{inst.version_display.minecraft_label}</span>
      <span class="cp-library-cell">
        {inst.version_display.supports_mods && inst.counts_available === true ? inst.mods_count : null}
      </span>
      <span class="cp-library-cell">{inst.last_played_at ? fmtRelative(inst.last_played_at) : null}</span>
      <span class="cp-library-acts">
        <Button
          size="sm"
          variant="ghost"
          icon={installing ? (install.state === 'queued' ? 'clock' : 'download') : actionIcon}
          disabled={installing}
          title={launchAction.primary_action === 'blocked' ? launchAction.disabled_reason : undefined}
          onClick={(e) => {
            e.stopPropagation();
            open();
          }}
        >
          {installing ? installLabel : launchAction.primary_action === 'launch' ? 'Play' : launchAction.label}
        </Button>
        <IconButton
          icon="dots"
          size={28}
          tooltip={`Actions for ${inst.name}`}
          onClick={(e) => {
            e.stopPropagation();
            onContextMenu(e);
          }}
        />
      </span>
    </div>
  );
}

export function InstancesView(): JSX.Element {
  const [view, setView] = useState<'grid' | 'list'>('grid');
  const [q, setQ] = useState('');
  const all = instances.value;
  const query = q.trim().toLowerCase();
  const filtered = all.filter((i) => i.name.toLowerCase().includes(query));
  const selection = useSelection(
    filtered,
    useCallback((inst: EnrichedInstance) => inst.id, []),
  );

  const openMenu = (e: MouseEvent, inst: EnrichedInstance): void => {
    openContextMenu(e, [
      selectionMenuItem(selection, inst.id),
      { divider: true, label: '', onSelect: () => undefined },
      ...instanceMenuItems(inst),
    ]);
  };

  return (
    <div class="cp-view-page cp-library">
      <header class="cp-library-head">
        <h1 class="cp-h1">Instances</h1>
        <span class="cp-head-fill" />
        <Input value={q} onChange={setQ} placeholder="Search instances" icon="search" />
        <Segmented<'grid' | 'list'>
          value={view}
          onChange={setView}
          ariaLabel="Library view"
          options={[
            { value: 'grid', label: 'Grid' },
            { value: 'list', label: 'List' },
          ]}
        />
        <Button size="lg" icon="plus" onClick={openCreate}>
          New instance
        </Button>
      </header>

      <PendingRemovalsNotice />
      {filtered.length === 0 ? (
        <Empty
          title={q ? 'No matches' : 'No instances yet'}
          actions={
            !q && (
              <Button icon="plus" onClick={openCreate}>
                New instance
              </Button>
            )
          }
        >
          {q ? 'Try a different search term.' : 'Create your first Minecraft instance to get started.'}
        </Empty>
      ) : view === 'grid' ? (
        <div class="cp-covers">
          {filtered.map((i) => (
            <InstanceCard
              key={i.id}
              inst={i}
              selected={selection.isSelected(i.id)}
              onToggleSelect={() => selection.toggle(i.id)}
              onContextMenu={(e) => openMenu(e, i)}
            />
          ))}
          <button type="button" class="cp-cover cp-cover--new" onClick={openCreate}>
            <span class="cp-cover-art">
              <Icon name="plus" size={24} />
            </span>
            <strong class="cp-cover-name">New instance</strong>
          </button>
        </div>
      ) : (
        <div class="cp-table cp-library-table">
          <div class="cp-tr cp-th">
            <span>Name</span>
            <span>Loader</span>
            <span>Minecraft</span>
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
      <SelectionActionTray
        selection={selection}
        itemLabel="instance"
        actions={[
          {
            label: 'Delete',
            icon: 'trash',
            danger: true,
            onClick: () => void deleteInstancesFlow(selection.selectedItems, selection.clear),
          },
        ]}
      />
    </div>
  );
}
