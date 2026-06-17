import type { JSX } from 'preact';
import { Icon } from '../../ui/Icons';
import type { ContextMenuItem } from '../../ui/ContextMenu';
import { ProfileSkinTile, SavedSkinTile } from './SkinTiles';
import { menuItemsForSavedSkin } from './saved-skin-menu';
import type { MinecraftCape, MinecraftProfile, MinecraftSkin, SavedSkinRecord, SkinVariant } from './types';

export function SavedSkinLibraryGrid({
  state,
  librarySkins,
  uploadDragActive,
  canUpload,
  showProfileSkinTile,
  minecraftProfile,
  profileSkin,
  profileSkinFileSrc,
  profileSkinVariant,
  profileCape,
  profileSkinIdentity,
  profilePreviewActive,
  profileMenuItems,
  selectedSkinTextureKey,
  previewExtraActive,
  skinActionsEnabled,
  currentProfileSavedKey,
  pendingApplyKey,
  deleteKey,
  editKey,
  applyKey,
  flushBusy,
  cancelPendingBusy,
  capeSrcForId,
  onOpenUploadPicker,
  onViewProfileSkin,
  onViewSavedSkin,
  onApplySkin,
  onFlushPendingApply,
  onCancelPendingApply,
  onStartEdit,
  onDownloadSavedSkin,
  onConfirmDeleteSkin,
}: {
  state: 'loading' | 'ready' | 'unavailable';
  librarySkins: SavedSkinRecord[];
  uploadDragActive: boolean;
  canUpload: boolean;
  showProfileSkinTile: boolean;
  minecraftProfile?: MinecraftProfile;
  profileSkin?: MinecraftSkin;
  profileSkinFileSrc?: string;
  profileSkinVariant: SkinVariant;
  profileCape?: MinecraftCape;
  profileSkinIdentity?: string;
  profilePreviewActive: boolean;
  profileMenuItems: ContextMenuItem[];
  selectedSkinTextureKey?: string | null;
  previewExtraActive: boolean;
  skinActionsEnabled: boolean;
  currentProfileSavedKey: string | null;
  pendingApplyKey: string | null;
  deleteKey: string | null;
  editKey: string | null;
  applyKey: string | null;
  flushBusy: boolean;
  cancelPendingBusy: boolean;
  capeSrcForId: (capeId: string | null | undefined) => string | undefined;
  onOpenUploadPicker: () => void;
  onViewProfileSkin: () => void;
  onViewSavedSkin: (textureKey: string) => void;
  onApplySkin: (textureKey: string) => void;
  onFlushPendingApply: () => void;
  onCancelPendingApply: () => void;
  onStartEdit: (skin: SavedSkinRecord) => void;
  onDownloadSavedSkin: (skin: SavedSkinRecord) => void;
  onConfirmDeleteSkin: (skin: SavedSkinRecord) => void;
}): JSX.Element {
  return (
    <section class="cp-skin-section" aria-label="Skin library">
      <header class="cp-skin-section__head">
        <span class="cp-skin-section__title">Library</span>
        {state === 'ready' && librarySkins.length > 0 && (
          <span class="cp-skin-section__count">{librarySkins.length}</span>
        )}
      </header>
      {state === 'loading' ? (
        <div class="cp-skin-grid-note">Loading saved skins...</div>
      ) : (
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

          {showProfileSkinTile && minecraftProfile && profileSkin && (
            <ProfileSkinTile
              minecraftProfile={minecraftProfile}
              profileSkinId={profileSkin.id}
              profileSkinUrl={profileSkin.url}
              profileSkinFileSrc={profileSkinFileSrc}
              profileSkinVariant={profileSkinVariant}
              profileCape={profileCape ?? null}
              profileSkinIdentity={profileSkinIdentity}
              selected={profilePreviewActive}
              menuItems={profileMenuItems}
              onView={onViewProfileSkin}
            />
          )}

          {librarySkins.map((skin) => {
            const applied = Boolean(
              skin.applied_at || (skinActionsEnabled && skin.texture_key === currentProfileSavedKey),
            );
            const selected = !previewExtraActive && selectedSkinTextureKey === skin.texture_key;
            const queued = pendingApplyKey === skin.texture_key;
            const deleting = deleteKey === skin.texture_key;
            const applyBlocked = applyKey === skin.texture_key || flushBusy || cancelPendingBusy;
            const pendingRowActionBusy = flushBusy || cancelPendingBusy || applyKey !== null;
            const tileMenuItems = menuItemsForSavedSkin({
              skin,
              applied,
              selectedPreviewEditing: editKey === skin.texture_key,
              skinActionsEnabled,
              applying: applyBlocked,
              pendingActionBusy: pendingRowActionBusy,
              queued,
              deleting,
              onView: () => onViewSavedSkin(skin.texture_key),
              onApply: () => onApplySkin(skin.texture_key),
              onApplyNow: onFlushPendingApply,
              onCancelQueue: onCancelPendingApply,
              onEdit: () => onStartEdit(skin),
              onDownload: () => onDownloadSavedSkin(skin),
              onDelete: () => onConfirmDeleteSkin(skin),
            });

            return (
              <SavedSkinTile
                key={skin.texture_key}
                skin={skin}
                selected={selected}
                queued={queued}
                applied={applied}
                deleting={deleting}
                capeSrc={capeSrcForId(skin.cape_id)}
                menuItems={tileMenuItems}
                onView={() => onViewSavedSkin(skin.texture_key)}
              />
            );
          })}
        </div>
      )}
    </section>
  );
}
