import { useComputed } from '@preact/signals';
import type { Instance, Version } from '../types';
import { collapsedGroups } from '../store';
import { local, saveLocalState } from '../state';
import { InstanceItem } from './InstanceItem';

interface InstanceGroupProps {
  groupKey: string;
  label: string;
  instances: Instance[];
  versionMap: Map<string, Version>;
  onContextMenu: (e: MouseEvent, inst: Instance) => void;
}

const CHEVRON_SVG = '<svg class="version-group-chevron" width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><polyline points="6 9 12 15 18 9"/></svg>';

export function InstanceGroup({ groupKey, label, instances, versionMap, onContextMenu }: InstanceGroupProps) {
  const collapsed = useComputed(() => !!collapsedGroups.value[groupKey]);

  const handleToggle = () => {
    const next = { ...collapsedGroups.value, [groupKey]: !collapsedGroups.value[groupKey] };
    collapsedGroups.value = next;
    local.collapsedGroups[groupKey] = next[groupKey];
    saveLocalState();
  };

  if (instances.length === 0) return null;

  return (
    <>
      <div
        class={`version-group-label${collapsed.value ? ' collapsed' : ''}`}
        data-group={groupKey}
        onClick={() => handleToggle()}
      >
        <span dangerouslySetInnerHTML={{ __html: CHEVRON_SVG }} />
        {label}{' '}
        <span style="opacity:.4;font-weight:400;margin-left:2px">{instances.length}</span>
      </div>
      <div
        class={`version-group-items${collapsed.value ? ' collapsed' : ''}`}
        data-group-items={groupKey}
      >
        {instances.map((inst, i) => (
          <InstanceItem
            key={inst.id}
            instance={inst}
            version={versionMap.get(inst.version_id)}
            index={i}
            onContextMenu={onContextMenu}
          />
        ))}
      </div>
    </>
  );
}
