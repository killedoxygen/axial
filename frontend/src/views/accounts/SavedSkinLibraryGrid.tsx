import type { JSX } from 'preact';
import { Icon } from '../../ui/Icons';
import type { ContextMenuItem } from '../../ui/ContextMenu';
import { ProfileSkinTile, SavedSkinTile } from './SkinTiles';
import type { LauncherAccount, SavedSkinRecord } from './types';

export function SavedSkinLibraryGrid({
  librarySkins,
  uploadDragActive,
  canUpload,
  profileAccount,
  profilePreviewActive,
  profileMenuItems,
  selectedSkinTextureKey,
  previewExtraActive,
  skinActionsEnabled,
  currentProfileSavedKey,
  pendingApplyKey,
  deletingKey,
  capeSrcForId,
  tileMenuItems,
  onOpenUploadPicker,
  onViewProfileSkin,
  onViewSavedSkin,
}: {
  librarySkins: SavedSkinRecord[];
  uploadDragActive: boolean;
  canUpload: boolean;
  /** Set when the profile's current skin gets its own tile. */
  profileAccount: LauncherAccount | null;
  profilePreviewActive: boolean;
  profileMenuItems: ContextMenuItem[];
  selectedSkinTextureKey?: string | null;
  previewExtraActive: boolean;
  skinActionsEnabled: boolean;
  currentProfileSavedKey: string | null;
  pendingApplyKey: string | null;
  deletingKey: string | null;
  capeSrcForId: (capeId: string | null | undefined) => string | undefined;
  tileMenuItems: (skin: SavedSkinRecord) => ContextMenuItem[];
  onOpenUploadPicker: () => void;
  onViewProfileSkin: () => void;
  onViewSavedSkin: (textureKey: string) => void;
}): JSX.Element {
  return (
    <div class="cp-skin-grid">
      <button
        type="button"
        class="cp-skin-add-tile"
        data-drag={uploadDragActive ? 'active' : 'idle'}
        disabled={!canUpload}
        onClick={onOpenUploadPicker}
        title="Upload a PNG skin file, or drop one here"
      >
        <Icon name="plus" size={24} />
        <span class="cp-skin-add-tile__label">Add skin</span>
        <span class="cp-skin-add-tile__hint">{uploadDragActive ? 'Drop to add' : 'Drag and drop'}</span>
      </button>

      {profileAccount && (
        <ProfileSkinTile
          account={profileAccount}
          selected={profilePreviewActive}
          menuItems={profileMenuItems}
          onView={onViewProfileSkin}
        />
      )}

      {librarySkins.map((skin) => {
        const applied = Boolean(skin.applied_at || (skinActionsEnabled && skin.texture_key === currentProfileSavedKey));
        const selected = !previewExtraActive && selectedSkinTextureKey === skin.texture_key;
        const queued = pendingApplyKey === skin.texture_key;
        const deleting = deletingKey === skin.texture_key;

        return (
          <SavedSkinTile
            key={skin.texture_key}
            skin={skin}
            selected={selected}
            queued={queued}
            applied={applied}
            deleting={deleting}
            capeSrc={capeSrcForId(skin.cape_id)}
            menuItems={tileMenuItems(skin)}
            onView={() => onViewSavedSkin(skin.texture_key)}
          />
        );
      })}
    </div>
  );
}
