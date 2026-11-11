import type { JSX } from 'preact';
import { Icon } from '../../ui/Icons';
import { capeFileUrl } from './api';
import { NO_CAPE_VALUE, type MinecraftCape } from './types';
import { WearTile } from './WearTile';

const isActive = (cape: MinecraftCape): boolean => cape.state.toLowerCase() === 'active';

/** "No cape", then the capes this Minecraft profile owns, the active one first. */
export function CapePicker({
  capes,
  value,
  busy = false,
  locked,
  onChange,
}: {
  capes: MinecraftCape[];
  value: string;
  busy?: boolean;
  /** Why the cape cannot be changed right now. */
  locked?: string;
  onChange: (value: string) => void;
}): JSX.Element {
  const sorted = [...capes].sort(
    (left, right) => Number(isActive(right)) - Number(isActive(left)) || left.id.localeCompare(right.id),
  );
  return (
    <div
      class="cp-wear-grid"
      role="radiogroup"
      aria-label="Cape"
      aria-busy={busy || undefined}
      aria-disabled={locked ? true : undefined}
      title={locked}
    >
      <WearTile
        role="radio"
        name="No cape"
        worn={value === NO_CAPE_VALUE}
        inHand={value === NO_CAPE_VALUE}
        disabled={busy || Boolean(locked)}
        onSelect={() => onChange(NO_CAPE_VALUE)}
      >
        <span class="cp-cape" data-none>
          <Icon name="x" size={16} />
        </span>
      </WearTile>
      {sorted.map((cape, index) => (
        <WearTile
          key={cape.id}
          role="radio"
          name={isActive(cape) ? 'Active cape' : `Cape ${index + 1}`}
          title={cape.id}
          worn={value === cape.id}
          inHand={value === cape.id}
          disabled={busy || Boolean(locked)}
          onSelect={() => onChange(cape.id)}
        >
          <span class="cp-cape">
            <img src={capeFileUrl(cape)} alt="" draggable={false} />
          </span>
        </WearTile>
      ))}
      {value !== NO_CAPE_VALUE && !capes.some((cape) => cape.id === value) && (
        <p class="cp-wear-note">The saved cape is not on this profile.</p>
      )}
    </div>
  );
}
