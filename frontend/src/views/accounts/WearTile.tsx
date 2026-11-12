import { toChildArray, type ComponentChildren, type JSX, type VNode } from 'preact';
import { useRef, useState } from 'preact/hooks';
import { Ring } from '../../ui/Atoms';
import { openContextMenu, type ContextMenuItem } from '../../ui/ContextMenu';
import { Icon } from '../../ui/Icons';
import { Popover } from '../../ui/Popover';

/**
 * Anything that can be worn, as one tile: a holder, then its name. The one worn has the neutral fill and the check;
 * the one in hand but not worn is outlined; a queued change carries the ring.
 */
interface WearTileProps {
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
}

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
}: WearTileProps): JSX.Element {
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

/**
 * Wearables as a grid of at most `limit` tiles. When there are more, or some are `stashed`, the last place goes to
 * "+N", which opens the rest in a popover; it carries the marks of what it holds, so the one worn or in hand is
 * never lost from sight.
 */
export function WearGrid({
  label,
  role = 'group',
  limit = Infinity,
  stashed,
  note,
  children,
  ...props
}: {
  label: string;
  role?: 'group' | 'radiogroup';
  limit?: number;
  /** Tiles that always wait behind "+N", after any that do not fit. */
  stashed?: ComponentChildren;
  /** A line under the tiles, outside the count. */
  note?: ComponentChildren;
  children: ComponentChildren;
} & Pick<JSX.HTMLAttributes<HTMLDivElement>, 'title' | 'aria-busy' | 'aria-disabled'>): JSX.Element {
  const tiles = toChildArray(children) as VNode<WearTileProps>[];
  const stash = toChildArray(stashed) as VNode<WearTileProps>[];
  const stacked = stash.length > 0 || tiles.length > limit;
  const shown = stacked ? tiles.slice(0, Math.max(0, limit - 1)) : tiles;
  return (
    <div class="cp-wear-grid" role={role} aria-label={label} {...props}>
      {shown}
      {stacked && <WearMore tiles={[...tiles.slice(shown.length), ...stash]} label={label} />}
      {note}
    </div>
  );
}

function WearMore({ tiles, label }: { tiles: VNode<WearTileProps>[]; label: string }): JSX.Element {
  const [open, setOpen] = useState(false);
  const anchor = useRef<HTMLDivElement>(null);
  const close = (): void => setOpen(false);
  return (
    <div ref={anchor} class="cp-wear-more">
      <WearTile
        name="More"
        title={`${tiles.length} more`}
        worn={tiles.some((tile) => tile.props.worn)}
        inHand={tiles.some((tile) => tile.props.inHand)}
        queued={tiles.some((tile) => tile.props.queued)}
        onSelect={() => setOpen(!open)}
      >
        <span class="cp-wear-count">+{tiles.length}</span>
      </WearTile>
      {open && (
        <Popover anchor={anchor} onDismiss={close} class="cp-pop cp-wear-pop">
          {/* A choice made here closes the list, as in the account row's "+N". */}
          <div class="cp-wear-grid" role="group" aria-label={`More ${label.toLowerCase()}`} onClick={close}>
            {tiles}
          </div>
        </Popover>
      )}
    </div>
  );
}
