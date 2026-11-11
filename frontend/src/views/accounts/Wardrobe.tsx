import type { JSX } from 'preact';
import { useMemo, useRef } from 'preact/hooks';
import { apiResourceUrl } from '../../api';
import { DEFAULT_SKINS } from '../../default-skins';
import {
  applySkin,
  cancelPendingApply,
  changeSavedSkinCape,
  deleteSavedSkin,
  downloadSavedSkin,
  flushPendingApply,
  inferredProfileSavedSkin,
  previewProfileSkin,
  resetProfileCape,
  resetProfileSkin,
  resetWardrobePreview,
  returnFromProfilePreview,
  savedDefaultSkin,
  saveProfileSkinLocally,
  selectDefaultSkin,
  selectSavedSkin,
  setWardrobeNotice,
  wardrobeContext,
  wardrobeData,
  wardrobeErrorMessage,
  wardrobeNotice,
  wardrobeOp,
  wardrobeSelection,
} from '../../machines/skin-wardrobe';
import { profileCape as accountProfileCape } from '../../player-skin';
import { Notice, Status } from '../../ui/Atoms';
import type { ContextMenuItem } from '../../ui/ContextMenu';
import { Icon } from '../../ui/Icons';
import {
  boundedMessage,
  capeFileUrl,
  DEFAULT_SKIN_SOURCE,
  savedSkinFileUrl,
  sortSavedSkins,
  stagedSkinPreviewSrc,
} from './api';
import { CapePicker } from './CapePicker';
import { SkinEditDialog } from './SkinEditDialog';
import { SkinFinder } from './SkinFinder';
import { savedSkinMenuItems } from './skin-menu';
import { SkinSnapshotImg } from './SkinSnapshotImg';
import { SkinStage, type SkinStageModel, type StageBack } from './SkinStage';
import { SkinUploadDialog } from './SkinUploadDialog';
import { NO_CAPE_VALUE, type SavedSkinRecord } from './types';
import { usePaletteFit } from './use-palette-fit';
import { useSavedSkinEditWorkflow } from './use-saved-skin-edit-workflow';
import { useSavedSkinLookupWorkflow } from './use-saved-skin-lookup-workflow';
import { useSavedSkinNativeDragDrop } from './use-saved-skin-native-drag-drop';
import { useSavedSkinUploadWorkflow } from './use-saved-skin-upload-workflow';
import { WearTile } from './WearTile';

/**
 * The active account's skins and capes: a narrow palette at the left (the player lookup, the skins, the capes of
 * the skin in hand), fitted to the page's height, and beside it the stage. The whole body takes a dropped PNG.
 */
export function Wardrobe({ skinActionDisabledReason }: { skinActionDisabledReason: string }): JSX.Element {
  const bodyRef = useRef<HTMLDivElement>(null);
  const data = wardrobeData.value;
  const selection = wardrobeSelection.value;
  const op = wardrobeOp.value;
  const { skinActionsEnabled: canWear, account } = wardrobeContext.value;

  const lookup = useSavedSkinLookupWorkflow();
  const edit = useSavedSkinEditWorkflow();
  const upload = useSavedSkinUploadWorkflow();

  const skins = data.skins;
  const savedByKey = useMemo(() => new Map(skins.map((skin) => [skin.texture_key, skin])), [skins]);
  const sorted = useMemo(() => sortSavedSkins(skins, 'recent'), [skins]);
  const ownSkins = sorted.filter((skin) => skin.source !== DEFAULT_SKIN_SOURCE);
  const capes = account?.minecraft_profile?.capes ?? [];
  const capeSrc = (capeId: string | null | undefined): string | undefined => {
    const cape = capeId ? capes.find((candidate) => candidate.id === capeId) : undefined;
    return cape ? capeFileUrl(cape) : undefined;
  };

  const pending = data.pendingApply;
  const queuedKey = pending?.phase === 'queued' || pending?.phase === 'applying' ? pending.texture_key : null;
  const profileSkin = account?.profile_skin;
  const profileCape = accountProfileCape(account);
  const profileSavedKey = inferredProfileSavedSkin()?.texture_key ?? null;
  const isWorn = (skin: SavedSkinRecord): boolean =>
    Boolean(skin.applied_at || (canWear && skin.texture_key === profileSavedKey));
  const wornSkin = skins.find(isWorn) ?? null;

  // What is in hand: a looked-up player, a built-in skin, the profile's own skin, or a saved one.
  const inHandDefault = selection.kind === 'default' ? DEFAULT_SKINS.find((skin) => skin.id === selection.id) : null;
  const inHandSaved =
    selection.kind === 'default'
      ? null
      : ((selection.kind === 'saved' ? savedByKey.get(selection.key) : null) ??
        (profileSavedKey ? savedByKey.get(profileSavedKey) : null) ??
        (queuedKey ? savedByKey.get(queuedKey) : null) ??
        skins.find((skin) => Boolean(skin.applied_at)) ??
        sorted[0] ??
        null);
  const lookedUp = selection.kind === 'lookup' && lookup.lookupState === 'ready' ? lookup.lookupProfile : null;
  const onProfile = selection.kind === 'profile' && Boolean(profileSkin);
  const onlyProfile = data.state === 'ready' && skins.length === 0 && Boolean(profileSkin);
  const savedInHand = !lookedUp && !inHandDefault && !onProfile ? inHandSaved : null;
  const savedOfDefault = inHandDefault ? savedDefaultSkin(inHandDefault, skins) : null;
  // A cape belongs to a saved skin; a built-in skin has one once it has a saved copy. Until then the capes
  // show what is worn and cannot be changed.
  const capeSkin = lookedUp || onProfile ? null : (savedInHand ?? savedOfDefault);

  const viewSaved = async (key: string): Promise<void> => {
    if (edit.editKey && edit.editKey !== key && !(await edit.closeSkinEditBeforeChanging())) return;
    selectSavedSkin(key);
  };
  const startEdit = async (skin: SavedSkinRecord): Promise<void> => {
    if (await edit.startEdit(skin)) selectSavedSkin(skin.texture_key);
  };
  const working = op?.kind === 'apply' || op?.kind === 'flush' || op?.kind === 'cancel-pending';
  const menuFor = (skin: SavedSkinRecord): ContextMenuItem[] =>
    op?.kind === 'delete' && op.key === skin.texture_key
      ? []
      : savedSkinMenuItems({
          worn: isWorn(skin),
          queued: queuedKey === skin.texture_key,
          editing: edit.editKey === skin.texture_key,
          canWear,
          busy: working,
          onWear: async () => {
            if (await edit.closeSkinEditBeforeChanging()) await applySkin(skin.texture_key);
          },
          onWearNow: () => void flushPendingApply(),
          onCancelQueue: () => void cancelPendingApply(),
          onEdit: () => void startEdit(skin),
          onDownload: () => void downloadSavedSkin(skin),
          onDelete: () => void deleteSavedSkin(skin),
        });

  const editing = savedInHand !== null && edit.editKey === savedInHand.texture_key;
  const stage: SkinStageModel = lookedUp
    ? { kind: 'lookup', profile: lookedUp, variant: lookup.lookupVariant }
    : inHandDefault
      ? { kind: 'default', skin: inHandDefault, worn: Boolean(savedOfDefault && isWorn(savedOfDefault)) }
      : onProfile || (!savedInHand && onlyProfile)
        ? { kind: 'profile' }
        : savedInHand
          ? {
              kind: 'saved',
              skin: savedInHand,
              capeSrc: capeSrc(savedInHand.cape_id),
              queued: savedInHand.texture_key === queuedKey,
              worn: isWorn(savedInHand),
              edit: editing
                ? {
                    src: edit.editReplacement ? stagedSkinPreviewSrc(edit.editReplacement) : null,
                    variant: edit.editVariant,
                    capeSrc: capeSrc(edit.editCapeId === NO_CAPE_VALUE ? null : edit.editCapeId),
                  }
                : null,
              menuItems: menuFor(savedInHand),
            }
          : data.state === 'ready'
            ? { kind: 'default', skin: DEFAULT_SKINS[0]!, worn: false }
            : { kind: 'empty', loading: data.state === 'loading' };

  // The profile's own skin gets a tile while no saved skin is known to be it.
  const profileTile = Boolean(profileSkin && !onlyProfile && !profileSavedKey);
  const back: StageBack | null =
    stage.kind === 'profile'
      ? inHandSaved && onProfile
        ? { label: `Back to ${inHandSaved.name}`, run: returnFromProfilePreview }
        : null
      : wornSkin && canWear
        ? { label: `Back to ${wornSkin.name}`, run: () => void viewSaved(wornSkin.texture_key) }
        : profileTile && canWear
          ? { label: 'Back to current skin', run: previewProfileSkin }
          : stage.kind === 'saved' || (stage.kind === 'default' && stage.skin.id !== 'steve')
            ? { label: 'Back to Steve', run: resetWardrobePreview }
            : null;

  useSavedSkinNativeDragDrop({
    dropSurfaceRef: bodyRef,
    editKey: edit.editKey,
    uploadBusy: op !== null,
    editBusy: Boolean(edit.editBusyKey || edit.editDetectBusyKey),
    setUploadDragActive: upload.setUploadDragActive,
    setEditReplacementDragActive: edit.setEditReplacementDragActive,
    notifyError: setWardrobeNotice,
    onReadError: (err) => setWardrobeNotice(wardrobeErrorMessage(err, 'Could not read dropped skin file.')),
    stageUploadFile: upload.stageUploadFile,
    stageEditReplacementFile: edit.stageEditReplacementFile,
  });

  const profileMenu: ContextMenuItem[] = [];
  if (canWear && profileSkin && op === null) {
    profileMenu.push(
      { icon: 'download', label: 'Save locally', onSelect: () => void saveProfileSkinLocally() },
      { icon: 'x', label: 'Reset profile skin', onSelect: () => void resetProfileSkin() },
    );
  }
  if (canWear && profileCape && op === null) {
    profileMenu.push({ icon: 'x', label: 'Reset profile cape', onSelect: () => void resetProfileCape() });
  }

  const problems = [
    lookup.lookupState === 'error' ? lookup.lookupError : lookup.lookupUsernameError,
    wardrobeNotice.value,
    pending?.error ? boundedMessage(pending.error, 'Minecraft profile apply failed.') : null,
    data.error ?? (data.state === 'unavailable' ? 'Saved skins are unavailable.' : null),
  ].filter((text): text is string => Boolean(text));

  const editingSkin = edit.editKey ? (savedByKey.get(edit.editKey) ?? null) : null;
  usePaletteFit(bodyRef, 1 + (profileTile ? 1 : 0) + ownSkins.length + DEFAULT_SKINS.length + capes.length);

  return (
    <>
      <input
        ref={edit.editTextureInputRef}
        type="file"
        accept="image/png"
        hidden
        onChange={(event) => {
          const file = event.currentTarget.files?.[0];
          if (file) edit.stageEditReplacementFile(file);
        }}
      />
      <input
        ref={upload.fileInputRef}
        type="file"
        accept="image/png"
        hidden
        onChange={(event) => {
          const file = event.currentTarget.files?.[0];
          if (file) upload.handleUploadInputFile(file);
        }}
      />

      <div
        ref={bodyRef}
        class="cp-wardrobe"
        data-drop={upload.uploadDragActive || undefined}
        onDragEnter={upload.uploadDrop.onDragEnter}
        onDragOver={upload.uploadDrop.onDragOver}
        onDragLeave={upload.uploadDrop.onDragLeave}
        onDrop={upload.uploadDrop.onDrop}
      >
        <div class="cp-palette">
          <div class="cp-palette-top">
            <SkinFinder
              username={lookup.lookupUsername}
              busy={lookup.lookupBusy && lookup.lookupState === 'loading'}
              previewing={lookedUp !== null}
              invalid={Boolean(lookup.lookupUsernameError) || lookup.lookupState === 'error'}
              onUsernameChange={lookup.handleLookupUsernameChange}
              onLookup={() => {
                if (lookup.canLookupSkin) void lookup.lookupSkin();
              }}
              onDismiss={lookup.dismissLookup}
            />
            {data.state === 'loading' && <Status busy>Loading saved skins</Status>}
            {problems.map((text) => (
              <Notice key={text} tone="error" role="alert">
                <p>{text}</p>
              </Notice>
            ))}
          </div>

          <div class="cp-wear-grid" role="group" aria-label="Skins">
            <WearTile
              add
              name={upload.uploadDragActive ? 'Drop to add' : 'Add skin'}
              title="Upload a PNG skin file, or drop one here"
              disabled={!upload.canUpload}
              onSelect={upload.openUploadPicker}
            >
              <Icon name="plus" size={20} stroke={2} />
            </WearTile>
            {profileTile && account && profileSkin && (
              <WearTile
                name="Current skin"
                title="The skin on your Minecraft profile"
                worn
                inHand={onProfile}
                menuItems={profileMenu}
                onSelect={previewProfileSkin}
              >
                <SkinSnapshotImg
                  cacheKey={`profile:${account.account_id}:${profileSkin.id}:${profileSkin.url}:${profileSkin.variant}:${profileCape?.id ?? ''}`}
                  src={apiResourceUrl('/skin/profile/file')}
                  variant={profileSkin.variant}
                  capeSrc={profileCape ? capeFileUrl(profileCape) : undefined}
                  textureIdentity={`profile:${account.account_id}:${profileSkin.id}:${profileSkin.url}`}
                  capeIdentity={profileCape?.id}
                  alt={`${account.display_name} current skin`}
                />
              </WearTile>
            )}
            {ownSkins.map((skin) => (
              <WearTile
                key={skin.texture_key}
                name={skin.name}
                title={skin.name}
                worn={isWorn(skin)}
                inHand={savedInHand?.texture_key === skin.texture_key}
                queued={queuedKey === skin.texture_key}
                disabled={op?.kind === 'delete' && op.key === skin.texture_key}
                menuItems={menuFor(skin)}
                onSelect={() => void viewSaved(skin.texture_key)}
              >
                <SkinSnapshotImg
                  cacheKey={`${skin.texture_key}:${skin.variant}:${skin.cape_id ?? ''}`}
                  src={savedSkinFileUrl(skin)}
                  variant={skin.variant}
                  capeSrc={capeSrc(skin.cape_id)}
                  textureIdentity={`saved:${skin.texture_key}`}
                  capeIdentity={skin.cape_id ?? undefined}
                  alt={`${skin.name} skin`}
                />
              </WearTile>
            ))}
            {DEFAULT_SKINS.map((skin) => {
              const saved = savedDefaultSkin(skin, skins);
              return (
                <WearTile
                  key={skin.id}
                  name={skin.name}
                  title={skin.name}
                  worn={Boolean(saved && isWorn(saved))}
                  inHand={inHandDefault?.id === skin.id || Boolean(saved && savedInHand === saved)}
                  queued={Boolean(saved && queuedKey === saved.texture_key)}
                  onSelect={() => selectDefaultSkin(skin.id)}
                >
                  <SkinSnapshotImg
                    cacheKey={`default:${skin.id}`}
                    src={skin.src}
                    variant={skin.variant}
                    alt={`${skin.name} skin`}
                  />
                </WearTile>
              );
            })}
          </div>

          {capes.length > 0 && (
            <CapePicker
              capes={capes}
              value={(capeSkin ?? wornSkin)?.cape_id ?? NO_CAPE_VALUE}
              busy={op?.kind === 'cape'}
              locked={capeSkin ? undefined : 'Save or wear this skin to choose its cape'}
              onChange={(value) =>
                capeSkin && void changeSavedSkinCape(capeSkin, value === NO_CAPE_VALUE ? null : value)
              }
            />
          )}
        </div>

        <SkinStage
          model={stage}
          back={back}
          onSaveLookup={(wear) => void lookup.saveUsernameSkin(wear)}
          onOpenUploadPicker={upload.openUploadPicker}
        />
      </div>

      <SkinUploadDialog upload={upload} capes={capes} capeSrc={capeSrc} disabledReason={skinActionDisabledReason} />
      <SkinEditDialog
        skin={editingSkin}
        edit={edit}
        capes={capes}
        capeSrc={capeSrc}
        deleting={op?.kind === 'delete' && op.key === editingSkin?.texture_key}
        disabledReason={skinActionDisabledReason}
      />
    </>
  );
}
