import type { JSX } from 'preact';
import { Button } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { apiResourceUrl } from '../../api';
import type { DefaultSkin } from '../../default-skins';
import { capeFileUrl, lookupCapeFileUrl, lookupSkinFileUrl, savedSkinFileUrl } from './api';
import { LazySkinThreePreview as SkinThreePreview } from './LazySkinThreePreview';
import type {
  AuthStatusState,
  MinecraftCape,
  MinecraftProfile,
  MinecraftSkin,
  MinecraftSkinLookup,
  SavedSkinRecord,
  SkinVariant,
} from './types';

export function SkinStage({
  state,
  onlineReady,
  lookupPreview,
  lookupVariant,
  lookupBusy,
  canSaveLookupSkin,
  onDismissLookup,
  onSaveUsernameSkin,
  stageDefaultSkin,
  selectedDefault,
  busy,
  canUpload,
  stageNametag,
  onRenameNametag,
  onResetPreview,
  onApplyDefaultSkin,
  profilePreviewActive,
  showProfileSelectedPreview,
  minecraftProfile,
  profileSkin,
  profileCape,
  profileSkinFileSrc,
  profileSkinVariant,
  profileBusy,
  canSaveProfileSkin,
  selectedSkin,
  selectedSkinCapeSrc,
  selectedQueued,
  selectedPreviewEditing,
  stageEditingSrc,
  editPreviewCapeSrc,
  editVariant,
  stageApplyBusy,
  cancelPendingBusy,
  flushBusy,
  applyKey,
  deleteKey,
  onReturnFromProfile,
  onSaveProfileSkin,
  onCancelPendingApply,
  onFlushPendingApply,
  onApplySkin,
  onStartEdit,
  onOpenUploadPicker,
}: {
  state: AuthStatusState;
  onlineReady: boolean;
  lookupPreview: MinecraftSkinLookup | null;
  lookupVariant: SkinVariant;
  lookupBusy: boolean;
  canSaveLookupSkin: boolean;
  onDismissLookup: () => void;
  onSaveUsernameSkin: (applyAfterSave: boolean) => void;
  stageDefaultSkin: DefaultSkin | null;
  selectedDefault: DefaultSkin | null;
  busy: boolean;
  canUpload: boolean;
  stageNametag: string | null;
  onRenameNametag?: () => void;
  onResetPreview: () => void;
  onApplyDefaultSkin: (skin: DefaultSkin) => void;
  profilePreviewActive: boolean;
  showProfileSelectedPreview: boolean;
  minecraftProfile?: MinecraftProfile;
  profileSkin: MinecraftSkin | null;
  profileCape: MinecraftCape | null;
  profileSkinFileSrc?: string;
  profileSkinVariant: SkinVariant;
  profileBusy: boolean;
  canSaveProfileSkin: boolean;
  selectedSkin: SavedSkinRecord | null;
  selectedSkinCapeSrc?: string;
  selectedQueued: boolean;
  selectedPreviewEditing: boolean;
  stageEditingSrc: string | null;
  editPreviewCapeSrc?: string;
  editVariant: SkinVariant;
  stageApplyBusy: boolean;
  cancelPendingBusy: boolean;
  flushBusy: boolean;
  applyKey: string | null;
  deleteKey: string | null;
  onReturnFromProfile: () => void;
  onSaveProfileSkin: () => void;
  onCancelPendingApply: () => void;
  onFlushPendingApply: () => void;
  onApplySkin: (textureKey: string) => void;
  onStartEdit: (skin: SavedSkinRecord) => void;
  onOpenUploadPicker: () => void;
}): JSX.Element {
  return (
    <section class="cp-skinstage" aria-label="Skin preview">
      {lookupPreview ? (
        <>
          <SkinThreePreview
            src={lookupSkinFileUrl(lookupPreview)}
            capeSrc={lookupCapeFileUrl(lookupPreview)}
            name={lookupPreview.username}
            nametag={lookupPreview.username}
            variant={lookupVariant}
            side="front"
            showOuterLayers
          />
          <div class="cp-skinstage__caption">{lookupPreview.username}'s current skin</div>
          <div class="cp-skinstage__actions">
            <Button
              variant="ghost"
              icon="x"
              disabled={lookupBusy}
              onClick={onDismissLookup}
              title="Stop previewing this player skin"
            >
              Dismiss
            </Button>
            {onlineReady ? (
              <>
                <Button
                  variant="secondary"
                  icon={lookupBusy ? 'refresh' : 'download'}
                  disabled={!canSaveLookupSkin}
                  onClick={() => onSaveUsernameSkin(false)}
                  title="Keep a copy in your library without wearing it"
                >
                  Save
                </Button>
                <Button
                  variant="primary"
                  icon={lookupBusy ? 'refresh' : 'check'}
                  disabled={!canSaveLookupSkin}
                  onClick={() => onSaveUsernameSkin(true)}
                  title="Save to your library and wear this skin"
                  sound="affirm"
                >
                  Apply
                </Button>
              </>
            ) : (
              <Button
                variant="primary"
                icon={lookupBusy ? 'refresh' : 'download'}
                disabled={!canSaveLookupSkin}
                onClick={() => onSaveUsernameSkin(false)}
                title="Keep a copy in your library"
                sound="affirm"
              >
                Save
              </Button>
            )}
          </div>
        </>
      ) : stageDefaultSkin ? (
        <>
          <SkinThreePreview
            src={stageDefaultSkin.src}
            name={stageDefaultSkin.name}
            nametag={stageNametag}
            onNametagEdit={onRenameNametag}
            variant={stageDefaultSkin.variant}
            side="front"
            showOuterLayers
          />
          <div class="cp-skinstage__caption">Minecraft default skin</div>
          <div class="cp-skinstage__actions">
            {selectedDefault && selectedDefault.id !== 'steve' && (
              <Button
                variant="secondary"
                size="lg"
                icon="refresh"
                disabled={busy}
                onClick={onResetPreview}
                title="Reset the account avatar to Steve"
              >
                Reset
              </Button>
            )}
            {onlineReady && (
              <Button
                variant="primary"
                size="lg"
                icon={busy ? 'refresh' : 'check'}
                disabled={!canUpload}
                onClick={() => onApplyDefaultSkin(stageDefaultSkin)}
                title="Wear this default skin on the active Minecraft account"
                sound="affirm"
              >
                {busy ? 'Applying' : 'Apply'}
              </Button>
            )}
          </div>
        </>
      ) : profilePreviewActive && minecraftProfile && profileSkin ? (
        <>
          <SkinThreePreview
            src={profileSkinFileSrc ?? apiResourceUrl('/skin/profile/file')}
            capeSrc={profileCape ? capeFileUrl(profileCape) : undefined}
            name={minecraftProfile.name}
            nametag={stageNametag}
            onNametagEdit={onRenameNametag}
            variant={profileSkinVariant}
            side="front"
            showOuterLayers
          />
          <div class="cp-skinstage__caption">Current Minecraft profile skin</div>
          <div class="cp-skinstage__actions">
            {selectedSkin && (
              <Button
                variant="secondary"
                size="lg"
                icon="refresh"
                disabled={profileBusy}
                onClick={onReturnFromProfile}
                title="Return to your skin"
              >
                Reset
              </Button>
            )}
            <Button
              variant="primary"
              size="lg"
              icon={profileBusy ? 'refresh' : 'download'}
              disabled={!canSaveProfileSkin}
              onClick={onSaveProfileSkin}
              title="Keep a copy of this skin in your library"
              sound="affirm"
            >
              Save
            </Button>
          </div>
        </>
      ) : selectedSkin ? (
        <>
          <SkinThreePreview
            src={stageEditingSrc ?? savedSkinFileUrl(selectedSkin)}
            capeSrc={selectedPreviewEditing ? editPreviewCapeSrc : selectedSkinCapeSrc}
            name={selectedSkin.name}
            nametag={stageNametag}
            onNametagEdit={onRenameNametag}
            variant={selectedPreviewEditing ? editVariant : selectedSkin.variant}
            side="front"
            showOuterLayers
          />
          <div class="cp-skinstage__actions">
            {selectedQueued ? (
              <>
                <Button
                  variant="secondary"
                  size="lg"
                  icon={cancelPendingBusy ? 'refresh' : 'x'}
                  disabled={cancelPendingBusy || flushBusy || applyKey !== null}
                  onClick={onCancelPendingApply}
                  title="Cancel the queued skin change"
                >
                  Cancel
                </Button>
                <Button
                  variant="primary"
                  size="lg"
                  icon={flushBusy ? 'refresh' : 'check'}
                  disabled={!onlineReady || flushBusy || cancelPendingBusy || applyKey !== null}
                  onClick={onFlushPendingApply}
                  title="Apply the queued skin change now"
                  sound="affirm"
                >
                  {flushBusy ? 'Applying' : 'Apply now'}
                </Button>
              </>
            ) : !selectedSkin.applied_at && !selectedPreviewEditing && onlineReady ? (
              <>
                <Button
                  variant="secondary"
                  size="lg"
                  icon="refresh"
                  disabled={stageApplyBusy}
                  onClick={onResetPreview}
                  title="Reset the account avatar to Steve"
                >
                  Reset
                </Button>
                <Button
                  variant="primary"
                  size="lg"
                  icon={stageApplyBusy ? 'refresh' : 'check'}
                  disabled={stageApplyBusy || flushBusy || cancelPendingBusy}
                  onClick={() => onApplySkin(selectedSkin.texture_key)}
                  title="Wear this skin on the active Minecraft account"
                  sound="affirm"
                >
                  {stageApplyBusy ? 'Applying' : 'Apply'}
                </Button>
              </>
            ) : !selectedPreviewEditing ? (
              <>
                <Button
                  variant="secondary"
                  size="lg"
                  icon="refresh"
                  disabled={deleteKey === selectedSkin.texture_key}
                  onClick={onResetPreview}
                  title="Reset the account avatar to Steve"
                >
                  Reset
                </Button>
                <Button
                  variant="secondary"
                  size="lg"
                  icon="edit"
                  disabled={deleteKey === selectedSkin.texture_key}
                  onClick={() => onStartEdit(selectedSkin)}
                >
                  Edit skin
                </Button>
              </>
            ) : null}
          </div>
        </>
      ) : showProfileSelectedPreview && minecraftProfile && profileSkin ? (
        <>
          <SkinThreePreview
            src={profileSkinFileSrc ?? apiResourceUrl('/skin/profile/file')}
            capeSrc={profileCape ? capeFileUrl(profileCape) : undefined}
            name={minecraftProfile.name}
            nametag={stageNametag}
            onNametagEdit={onRenameNametag}
            variant={profileSkinVariant}
            side="front"
            showOuterLayers
          />
          <div class="cp-skinstage__caption">Current Minecraft profile skin</div>
          <div class="cp-skinstage__actions">
            <Button
              variant="primary"
              size="lg"
              icon={profileBusy ? 'refresh' : 'download'}
              disabled={!canSaveProfileSkin}
              onClick={onSaveProfileSkin}
              title="Keep a copy of this skin in your library"
              sound="affirm"
            >
              Save
            </Button>
          </div>
        </>
      ) : (
        <div class="cp-skinstage__empty">
          <Icon name="image" size={28} color="var(--text-mute)" />
          <span>{state === 'loading' ? 'Loading skins' : 'No skin selected yet'}</span>
          {state !== 'loading' && (
            <Button variant="secondary" icon="plus" disabled={!canUpload} onClick={onOpenUploadPicker}>
              Add skin
            </Button>
          )}
        </div>
      )}
    </section>
  );
}
