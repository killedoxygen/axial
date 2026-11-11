import type { JSX } from 'preact';
import { IconButton } from '../../ui/Atoms';
import { openContextMenu, type ContextMenuItem } from '../../ui/ContextMenu';
import { Icon } from '../../ui/Icons';
import type { DefaultSkin } from '../../default-skins';
import { apiResourceUrl } from '../../api';
import { capeFileUrl, savedSkinFileUrl } from './api';
import { SkinSnapshotImg } from './SkinSnapshotImg';
import { profileCape } from '../../player-skin';
import type { LauncherAccount, SavedSkinRecord } from './types';

/** The skin the account's Minecraft profile wears. Renders nothing while it has none. */
export function ProfileSkinTile({
  account,
  selected,
  menuItems,
  onView,
}: {
  account: LauncherAccount;
  selected: boolean;
  menuItems: ContextMenuItem[];
  onView: () => void;
}): JSX.Element | null {
  const skin = account.profile_skin;
  if (!skin) return null;
  const cape = profileCape(account);
  const identity = `profile:${account.account_id}:${skin.id}:${skin.url}`;
  return (
    <div class="cp-skin-tilewrap">
      <button
        type="button"
        class="cp-skin-tile"
        data-kind="profile"
        data-selected={selected ? 'true' : 'false'}
        aria-pressed={selected}
        onClick={onView}
        onContextMenu={(event) => openContextMenu(event, menuItems)}
        title="Preview the current Minecraft profile skin"
      >
        <SkinSnapshotImg
          cacheKey={`${identity}:${skin.variant}:${cape?.id ?? ''}`}
          src={apiResourceUrl('/skin/profile/file')}
          variant={skin.variant}
          capeSrc={cape ? capeFileUrl(cape) : undefined}
          textureIdentity={identity}
          capeIdentity={cape?.id}
          alt={`${account.display_name} current profile skin`}
        />
        <span class="cp-skin-tile__label">Current profile</span>
      </button>
    </div>
  );
}

export function SavedSkinTile({
  skin,
  selected,
  queued,
  applied,
  deleting,
  capeSrc,
  menuItems,
  onView,
}: {
  skin: SavedSkinRecord;
  selected: boolean;
  queued: boolean;
  applied: boolean;
  deleting: boolean;
  capeSrc?: string;
  menuItems: ContextMenuItem[];
  onView: () => void;
}): JSX.Element {
  return (
    <div class="cp-skin-tilewrap">
      <button
        type="button"
        class="cp-skin-tile"
        data-selected={selected ? 'true' : 'false'}
        aria-pressed={selected}
        disabled={deleting}
        onClick={onView}
        onContextMenu={menuItems.length === 0 ? undefined : (event) => openContextMenu(event, menuItems)}
        title={skin.name}
      >
        <SkinSnapshotImg
          cacheKey={`${skin.texture_key}:${skin.variant}:${skin.cape_id ?? ''}`}
          src={savedSkinFileUrl(skin)}
          variant={skin.variant}
          capeSrc={capeSrc}
          textureIdentity={`saved:${skin.texture_key}`}
          capeIdentity={skin.cape_id ?? undefined}
          alt={`${skin.name} skin`}
        />
        {(queued || (applied && !selected)) && (
          <span
            class="cp-skin-tile__state"
            data-state={queued ? 'queued' : 'equipped'}
            title={queued ? 'Queued for apply' : 'Equipped on the Minecraft profile'}
          >
            <Icon name={queued ? 'refresh' : 'check'} size={11} stroke={2.4} />
          </span>
        )}
        <span class="cp-skin-tile__label">{skin.name}</span>
      </button>
      <span class="cp-skin-tilewrap__menu">
        <IconButton
          icon="dots"
          size={26}
          tooltip="Skin actions"
          disabled={menuItems.length === 0}
          onClick={(event) => {
            event.stopPropagation();
            openContextMenu(event, menuItems);
          }}
        />
      </span>
    </div>
  );
}

export function DefaultSkinTile({
  skin,
  selected,
  queued,
  applied,
  onView,
}: {
  skin: DefaultSkin;
  selected: boolean;
  queued: boolean;
  applied: boolean;
  onView: () => void;
}): JSX.Element {
  return (
    <button
      type="button"
      class="cp-skin-tile cp-skin-tile--compact"
      data-selected={selected ? 'true' : 'false'}
      aria-pressed={selected}
      onClick={onView}
      title={skin.name}
    >
      <SkinSnapshotImg
        cacheKey={`default:${skin.id}`}
        src={skin.src}
        variant={skin.variant}
        alt={`${skin.name} default skin`}
      />
      {(queued || (applied && !selected)) && (
        <span
          class="cp-skin-tile__state"
          data-state={queued ? 'queued' : 'equipped'}
          title={queued ? 'Queued for apply' : 'Equipped on the Minecraft profile'}
        >
          <Icon name={queued ? 'refresh' : 'check'} size={11} stroke={2.4} />
        </span>
      )}
      <span class="cp-skin-tile__label">{skin.name}</span>
    </button>
  );
}
