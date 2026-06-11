import type { JSX } from 'preact';
import { Icon } from '../../../ui/Icons';
import { Button, Card } from '../../../ui/Atoms';
import { openContextMenu } from '../../../ui/ContextMenu';
import type { EnrichedInstance, InstanceResourceSummary } from '../../../types';
import { fmtBytes, fmtRelative } from '../format';
import { openInstanceFolder } from '../instance-actions';
import { worldMenuItems } from '../world-actions';

function WorldsEmptyArt(): JSX.Element {
  return (
    <svg class="cp-od-worlds-svg" xmlns="http://www.w3.org/2000/svg" viewBox="0 0 180 172.3" aria-hidden="true">
      <polygon fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="21.3 30.9 34.5 24.3 47.7 30.9 47.7 45.7 34.5 52.3 21.3 45.7" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="21.3 30.9 34.5 37.5 47.7 30.9" />
      <line fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" x1="34.5" x2="34.5" y1="37.5" y2="52.3" />
      <polygon fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="44.3 58.3 57.5 51.7 70.7 58.3 70.7 73.1 57.5 79.7 44.3 73.1" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="44.3 58.3 57.5 64.9 70.7 58.3" />
      <line fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" x1="57.5" x2="57.5" y1="64.9" y2="79.7" />
      <polygon fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="81.0 16.7 90.6 2.3 100.2 16.7 90.6 21.5" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="83.1 17.8 75.6 29.0 90.6 36.5 105.6 29.0 98.1 17.8" />
      <polygon fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="78.1 30.3 70.6 41.5 90.6 51.5 110.6 41.5 103.1 30.3 90.6 36.5" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="86.6 49.5 86.6 56.2 90.6 58.2 94.6 56.2 94.6 49.5" />
      <line fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" x1="90.6" x2="90.6" y1="51.5" y2="58.2" />
      <polygon class="cp-od-worlds-accent" fill="none" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="122.4 29.7 135.6 23.1 148.8 29.7 148.8 44.5 135.6 51.1 122.4 44.5" />
      <polyline class="cp-od-worlds-accent" fill="none" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="122.4 29.7 135.6 36.3 148.8 29.7" />
      <line class="cp-od-worlds-accent" fill="none" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" x1="135.6" x2="135.6" y1="36.3" y2="51.1" />
      <polygon fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="108.9 64.8 122.1 58.2 135.3 64.8 135.3 79.6 122.1 86.2 108.9 79.6" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="108.9 64.8 122.1 71.4 135.3 64.8" />
      <line fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" x1="122.1" x2="122.1" y1="71.4" y2="86.2" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-miterlimit="10" points="28.6 50.1 13.7 55.6 2.5 73 2.5 94.2 8.3 98.3 18.3 126.4 33.7 132.3 51.7 154.9 71.9 148.3 83.9 158.3 95.9 169.8 117 150.6 134.6 147.7 149.6 126.1 161.5 120.5 171.3 96.2 177.9 91 177.8 71.5 167.9 60.2 166.7 54.3 147.3 47.9" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linecap="round" stroke-miterlimit="10" points="2.7 73.3 24.7 87.8 43.4 96 46.7 98.4 68.5 96.5 106.5 102.7 119 95.5 122.5 93.8 152.6 90.4 154.8 87.3 177.8 71.8" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linecap="round" stroke-miterlimit="10" points="8.6 98.5 25.9 107.1 46.6 114.9 55.1 114 75.7 135.9 96.3 119.8 106.5 119.9 106.5 102.7" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linecap="round" stroke-miterlimit="10" points="152.6 90.4 152.6 107.1 144.5 109.9 138 120.5 124.1 129.1 116.9 150.4" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linecap="round" stroke-linejoin="round" stroke-miterlimit="10" points="75.9 135.9 83.9 158.3 84 158.5" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linejoin="round" stroke-miterlimit="10" points="24.9 88 25.7 107.1 25.8 107.1" />
      <polyline fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linecap="round" stroke-linejoin="round" stroke-miterlimit="10" points="46.6 98.4 46.5 114.7 46.6 114.9" />
      <line fill="none" stroke="#808184" stroke-width="0.6179" stroke-miterlimit="10" x1="25.9" x2="33.9" y1="107.3" y2="132.3" />
      <line fill="none" stroke="#808184" stroke-width="0.6179" stroke-miterlimit="10" x1="33.9" x2="46.4" y1="132.3" y2="115.1" />
      <polyline fill="none" stroke="#808184" stroke-width="0.6179" stroke-linecap="round" stroke-miterlimit="10" points="106.7 120 124 128.9 118.1 107.3 118.3 95.7" />
      <line fill="none" stroke="#808184" stroke-width="0.6179" stroke-miterlimit="10" x1="106.9" x2="117.9" y1="119.3" y2="107.3" />
      <polyline fill="none" stroke="#808184" stroke-width="0.6179" stroke-linecap="round" stroke-miterlimit="10" points="149.6 125.6 152.4 107.3 171.3 96.4" />
      <path fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-linecap="square" stroke-miterlimit="10" d="m50.4 44.4 6.2-1.7 15.4-0.1" />
      <path fill="none" stroke="#9C9EA2" stroke-width="0.75" stroke-miterlimit="10" d="m110.9 42.5 8.8 0.7" />
      <polygon fill="none" stroke="#808184" stroke-width="0.6179" stroke-linecap="round" stroke-linejoin="round" stroke-miterlimit="10" points="18.1 68.7 26.3 64.6 34.5 68.7 34.5 71.2 26.3 75.3 18.1 71.2" />
      <polyline fill="none" stroke="#808184" stroke-width="0.6179" stroke-linecap="round" stroke-linejoin="round" stroke-miterlimit="10" points="18.1 68.7 26.3 72.8 34.5 68.7" />
      <line fill="none" stroke="#808184" stroke-width="0.6179" stroke-linecap="round" stroke-linejoin="round" stroke-miterlimit="10" x1="26.3" x2="26.3" y1="72.8" y2="75.3" />
      <polygon fill="none" stroke="#808184" stroke-width="0.6179" stroke-linecap="round" stroke-linejoin="round" stroke-miterlimit="10" points="77.8 82.7 86.0 78.6 94.2 82.7 94.2 85.2 86.0 89.3 77.8 85.2" />
      <polyline fill="none" stroke="#808184" stroke-width="0.6179" stroke-linecap="round" stroke-linejoin="round" stroke-miterlimit="10" points="77.8 82.7 86.0 86.8 94.2 82.7" />
      <line fill="none" stroke="#808184" stroke-width="0.6179" stroke-linecap="round" stroke-linejoin="round" stroke-miterlimit="10" x1="86.0" x2="86.0" y1="86.8" y2="89.3" />
      <polygon fill="none" stroke="#808184" stroke-width="0.6179" stroke-linecap="round" stroke-linejoin="round" stroke-miterlimit="10" points="141.1 63.0 149.3 58.9 157.5 63.0 157.5 65.5 149.3 69.6 141.1 65.5" />
      <polyline fill="none" stroke="#808184" stroke-width="0.6179" stroke-linecap="round" stroke-linejoin="round" stroke-miterlimit="10" points="141.1 63.0 149.3 67.1 157.5 63.0" />
      <line fill="none" stroke="#808184" stroke-width="0.6179" stroke-linecap="round" stroke-linejoin="round" stroke-miterlimit="10" x1="149.3" x2="149.3" y1="67.1" y2="69.6" />
    </svg>
  );
}

export function WorldsCard({
  inst,
  resources,
  onOpenWorlds,
  onRefresh,
}: {
  inst: EnrichedInstance;
  resources: InstanceResourceSummary | null;
  onOpenWorlds: () => void;
  onRefresh: () => void;
}): JSX.Element {
  const worlds = resources?.worlds ?? [];
  const count = resources
    ? Math.max(resources.worlds_count ?? 0, worlds.length)
    : inst.saves_count ?? 0;
  const visibleWorlds = worlds.slice(0, 3);
  const hiddenWorlds = visibleWorlds.length > 0 ? Math.max(count - visibleWorlds.length, 0) : 0;
  const footerCopy = visibleWorlds.length === 0
    ? 'Open the Worlds tab to see saves'
    : hiddenWorlds > 0
      ? `${hiddenWorlds} more world${hiddenWorlds === 1 ? '' : 's'} in Worlds`
      : `${count} world${count === 1 ? '' : 's'} available`;
  return (
    <Card padding={18} class={`cp-od-worlds-card${count === 0 ? ' cp-od-worlds-card--empty' : ''}`}>
      {count === 0 ? (
        <div class="cp-od-worlds-empty">
          <div class="cp-od-worlds-art" aria-hidden="true">
            <WorldsEmptyArt />
          </div>
          <div class="cp-od-worlds-lead">
            <div class="cp-od-worlds-copy">
              <h4>No worlds yet</h4>
              <p>Create a new world, import an existing save,<br />or launch Minecraft and create one there.</p>
            </div>
          </div>
          <div class="cp-od-worlds-cta">
            <Button icon="globe" onClick={onOpenWorlds} sound="affirm">View worlds</Button>
            <Button variant="ghost" icon="folder" onClick={() => void openInstanceFolder(inst.id, 'saves')}>Import world</Button>
          </div>
        </div>
      ) : (
        <div class="cp-od-worlds-list">
          {visibleWorlds.length > 0 ? visibleWorlds.map((world) => (
            <div
              class="cp-od-world-row"
              key={world.name}
              onContextMenu={(e) => openContextMenu(e, worldMenuItems(inst, world.name, onRefresh))}
            >
              <div class="cp-od-world-mark"><Icon name="globe" size={16} /></div>
              <div class="cp-od-world-body">
                <div class="cp-od-world-name" title={world.name}>{world.name}</div>
                <div class="cp-od-world-sub">{fmtBytes(world.size)} · changed {fmtRelative(world.modified_at)}</div>
              </div>
            </div>
          )) : (
            <div class="cp-od-world-row">
              <div class="cp-od-world-mark"><Icon name="globe" size={16} /></div>
              <div class="cp-od-world-body">
                <div class="cp-od-world-name">{count} save{count === 1 ? '' : 's'} on disk</div>
                <div class="cp-od-world-sub">Last touched {fmtRelative(inst.last_played_at)}</div>
              </div>
            </div>
          )}
          <div class="cp-od-worlds-footer">
            <span>{footerCopy}</span>
            <button class="cp-od-link" type="button" onClick={onOpenWorlds}>
              View worlds <Icon name="chevron-right" size={11} stroke={2.2} />
            </button>
          </div>
        </div>
      )}
    </Card>
  );
}
