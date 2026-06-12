import type { JSX } from 'preact';
import { Card } from '../../../ui/Atoms';
import { versionById } from '../../../store';
import { minecraftVersionLabel } from '../../../version-display';
import type { EnrichedInstance } from '../../../types';
import { loaderKeyFromVersion, LOADER_LABELS } from '../../create/defaults';
import { fmtJoined, fmtRelative } from '../format';

export function DetailsCard({ inst, running }: { inst: EnrichedInstance; running: boolean }): JSX.Element {
  const v = versionById(inst.version_id);
  const loader = LOADER_LABELS[loaderKeyFromVersion(v)];
  const loaderVer = v?.loader?.loader_version ? ` ${v.loader.loader_version}` : '';
  const mcVer = minecraftVersionLabel(v);
  return (
    <Card padding={18}>
      <div class="cp-od-head">
        <h3>Details</h3>
      </div>
      <div class="cp-od-kv">
        <div class="cp-od-kv-row">
          <span class="cp-od-kv-key">Status</span>
          <span class="cp-od-kv-val">
            <span class="cp-od-status" data-running={running}>
              <span class="cp-od-status-dot" aria-hidden="true" />
              {running ? 'Running' : 'Ready'}
            </span>
          </span>
        </div>
        <div class="cp-od-kv-row">
          <span class="cp-od-kv-key">Minecraft</span>
          <span class="cp-od-kv-val cp-od-kv-val--mono">{mcVer}</span>
        </div>
        <div class="cp-od-kv-row">
          <span class="cp-od-kv-key">Loader</span>
          <span class="cp-od-kv-val">{loader}{loaderVer}</span>
        </div>
        <div class="cp-od-kv-row">
          <span class="cp-od-kv-key">Created</span>
          <span class="cp-od-kv-val">{fmtJoined(inst.created_at)}</span>
        </div>
        <div class="cp-od-kv-row">
          <span class="cp-od-kv-key">Last played</span>
          <span class="cp-od-kv-val">{fmtRelative(inst.last_played_at)}</span>
        </div>
      </div>
    </Card>
  );
}
