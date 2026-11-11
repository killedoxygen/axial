import type { ComponentChildren, JSX } from 'preact';
import { Ring } from '../../ui/Atoms';
import { openContextMenu, type ContextMenuItem } from '../../ui/ContextMenu';
import { Icon } from '../../ui/Icons';

/**
 * Anything that can be worn, as one tile: a holder, then its name. The one worn has the neutral fill and the check;
 * the one in hand but not worn is outlined; a queued change carries the ring.
 */
export function WearTile({
  name,
  title,
  worn = false,
  inHand = false,
  queued = false,
  add = false,
  disabled,
  menuItems,
  role,
  onSelect,
  children,
}: {
  name: string;
  title?: string;
  worn?: boolean;
  inHand?: boolean;
  queued?: boolean;
  /** The way to add one: an accent plus where the art would stand. */
  add?: boolean;
  disabled?: boolean;
  menuItems?: ContextMenuItem[];
  /** A choice of one (capes) rather than something put on the stage (skins). */
  role?: 'radio';
  onSelect: () => void;
  children: ComponentChildren;
}): JSX.Element {
  const menu = menuItems && menuItems.length > 0 ? menuItems : null;
  return (
    <button
      type="button"
      class="cp-wear"
      role={role}
      aria-checked={role ? inHand : undefined}
      aria-pressed={role || add ? undefined : inHand}
      data-in-hand={inHand}
      data-worn={worn}
      data-add={add || undefined}
      disabled={disabled}
      title={title}
      onClick={onSelect}
      onContextMenu={menu ? (event) => openContextMenu(event, menu) : undefined}
    >
      <span class="cp-wear-holder">{children}</span>
      <span class="cp-wear-name">{name}</span>
      {queued ? (
        <span class="cp-wear-mark" data-queued title="Queued for the next launch">
          <Ring size={12} />
        </span>
      ) : (
        worn && (
          <span class="cp-wear-mark" title="Wearing">
            <Icon name="check" size={11} stroke={3} />
          </span>
        )
      )}
    </button>
  );
}
