import { useComputed } from '@preact/signals';
import type { Instance, Version } from '../types';
import { bootstrapError, bootstrapState, filteredInstances, instances, versionMap } from '../store';
import { showInstanceContextMenu } from '../context-menu';
import { isReleaseVersion, isSnapshotVersion } from '../utils';
import { InstanceGroup } from './InstanceGroup';

interface InstanceGroups {
  release: Instance[];
  modded: Instance[];
  snapshot: Instance[];
  other: Instance[];
}

export function InstanceList() {
  const boot = bootstrapState.value;
  const error = bootstrapError.value;
  const empty = useComputed(() => instances.value.length === 0);
  const filtered = useComputed(() => filteredInstances.value);
  const vm = useComputed(() => versionMap.value);

  const groups = useComputed<InstanceGroups>(() => {
    const map = vm.value;
    const result: InstanceGroups = { release: [], modded: [], snapshot: [], other: [] };
    for (const inst of filtered.value) {
      const v: Version | undefined = map.get(inst.version_id);
      if (v?.inherits_from) result.modded.push(inst);
      else if (isReleaseVersion(v)) result.release.push(inst);
      else if (isSnapshotVersion(v)) result.snapshot.push(inst);
      else result.other.push(inst);
    }
    return result;
  });

  const handleContextMenu = (e: MouseEvent, inst: Instance) => {
    showInstanceContextMenu(e, inst);
  };

  if (boot === 'loading') {
    return (
      <div class="loading-placeholder">
        <div class="spinner" />
        <span>Scanning versions...</span>
      </div>
    );
  }

  if (boot === 'error') {
    return (
      <div class="loading-placeholder">
        <span style="color:var(--red)">Failed to connect</span>
        <span style="color:var(--text-muted);font-size:10px">{error || 'Unknown error'}</span>
      </div>
    );
  }

  if (empty.value) {
    return (
      <div class="loading-placeholder">
        <span>No instances</span>
      </div>
    );
  }

  if (filtered.value.length === 0) {
    return (
      <div class="loading-placeholder">
        <span>No matching instances</span>
      </div>
    );
  }

  const g = groups.value;
  const m = vm.value;

  return (
    <>
      <InstanceGroup
        groupKey="release"
        label="Releases"
        instances={g.release}
        versionMap={m}
        onContextMenu={handleContextMenu}
      />
      <InstanceGroup
        groupKey="modded"
        label="Modded"
        instances={g.modded}
        versionMap={m}
        onContextMenu={handleContextMenu}
      />
      <InstanceGroup
        groupKey="snapshot"
        label="Snapshots"
        instances={g.snapshot}
        versionMap={m}
        onContextMenu={handleContextMenu}
      />
      <InstanceGroup
        groupKey="other"
        label="Other"
        instances={g.other}
        versionMap={m}
        onContextMenu={handleContextMenu}
      />
    </>
  );
}
