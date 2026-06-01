import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
import { Icon } from '../../../ui/Icons';
import { Button } from '../../../ui/Atoms';
import type { EnrichedInstance } from '../../../types';
import { fmtBytes } from '../format';
import type { ResourceLoadState } from '../resources';
import { openInstanceFolder } from '../instance-actions';
import { ResourceStatus } from '../components/resource-bits';

type ModFilter = 'all' | 'enabled' | 'disabled';

export function ModsPane({
  inst,
  resources,
  onRefresh,
}: {
  inst: EnrichedInstance;
  resources: ResourceLoadState;
  onRefresh: () => void;
}): JSX.Element {
  const [q, setQ] = useState('');
  const [filter, setFilter] = useState<ModFilter>('all');
  const mods = resources.data?.mods ?? [];
  const filteredMods = mods.filter((mod) => {
    const matchesSearch = mod.name.toLowerCase().includes(q.trim().toLowerCase());
    const matchesFilter = filter === 'all' || (filter === 'enabled' ? mod.enabled : !mod.enabled);
    return matchesSearch && matchesFilter;
  });

  return (
    <div class="cp-instance-body" style={{ display: 'block' }}>
      <div class="cp-mods-toolbar">
        <div class="cp-mods-search">
          <Icon name="search" size={14} color="var(--text-mute)" />
          <input
            type="text"
            placeholder="Filter mods…"
            value={q}
            autocomplete="off"
            spellcheck={false}
            onInput={(e: any) => setQ(e.currentTarget.value)}
          />
        </div>
        <div class="cp-mini-seg" role="tablist" aria-label="Filter mods">
          {(['all', 'enabled', 'disabled'] as ModFilter[]).map(f => (
            <button
              key={f}
              type="button"
              role="tab"
              aria-selected={filter === f}
              data-active={filter === f}
              onClick={() => setFilter(f)}
            >
              {f[0].toUpperCase() + f.slice(1)}
            </button>
          ))}
        </div>
        <Button variant="secondary" size="sm" icon="refresh" onClick={onRefresh}>Refresh</Button>
        <Button
          variant="soft"
          size="sm"
          icon="plus"
          onClick={() => void openInstanceFolder(inst.id, 'mods')}
        >
          Add mod
        </Button>
      </div>
      <div class="cp-mods-table">
        <div class="cp-mods-table-head" aria-hidden="true">
          <span /><span />
          <span>Name</span>
          <span>Category</span>
          <span>Version</span>
          <span>State</span>
          <span />
        </div>
        <ResourceStatus state={resources} onRetry={onRefresh} />
        {resources.status !== 'loading' && filteredMods.length === 0 ? (
          <div class="cp-mods-empty-row">
            <strong>{mods.length === 0 ? 'No mods installed in this instance' : 'No mods match this filter'}</strong>
            Drop jar files into the mods folder. In-app mod browsing and metadata are still backend-team work.
          </div>
        ) : (
          filteredMods.map((mod) => (
            <div class="cp-mods-table-row" data-disabled={!mod.enabled} key={mod.name}>
              <span><Icon name="puzzle" size={15} color="var(--text-dim)" /></span>
              <span class="cp-mods-file-icon">JAR</span>
              <span class="cp-resource-name" title={mod.name}>{mod.name}</span>
              <span>Local</span>
              <span>{fmtBytes(mod.size)}</span>
              <span>{mod.enabled ? 'Enabled' : 'Disabled'}</span>
              <span />
            </div>
          ))
        )}
      </div>
    </div>
  );
}
