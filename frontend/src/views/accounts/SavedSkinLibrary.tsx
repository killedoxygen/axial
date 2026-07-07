import type { JSX } from 'preact';
import { useMemo, useRef } from 'preact/hooks';
import { apiResourceUrl } from '../../api';
import { DEFAULT_SKINS } from '../../default-skins';
import {
  applySkin,
  cancelPendingApply,
  changeSavedSkinCape,
  defaultSkinKeys,
  defaultSkinKeysReady,
  deleteSavedSkin,
  downloadSavedSkin,
  flushPendingApply,
  inferredProfileSavedSkin,
  previewProfileSkin,
  resetProfileCape,
  resetProfileSkin,
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
import { local } from '../../state';
import type { ContextMenuItem } from '../../ui/ContextMenu';
import {
  activeMinecraftCape,
  activeMinecraftSkin,
  capeFileUrl,
  DEFAULT_SKIN_SOURCE,
  skinVariantValue,
  sortSavedSkins,
  stagedSkinPreviewSrc,
} from './api';
import { AccountSwitcherChip } from './AccountSwitcher';
import { menuItemsForSavedSkin } from './saved-skin-menu';
import { SavedSkinCapeSection } from './SavedSkinCapeSection';
import { SavedSkinDefaultStrip } from './SavedSkinDefaultStrip';
import { SavedSkinFileInputs } from './SavedSkinFileInputs';
import { SavedSkinLibraryGrid } from './SavedSkinLibraryGrid';
import { SkinEditDialog } from './SkinEditDialog';
import { SkinFinder } from './SkinFinder';
import { SkinStage, type SkinStageModel } from './SkinStage';
import { SkinUploadDialog } from './SkinUploadDialog';
import { useSavedSkinEditWorkflow } from './use-saved-skin-edit-workflow';
import { useSavedSkinLookupWorkflow } from './use-saved-skin-lookup-workflow';
import { useSavedSkinNativeDragDrop } from './use-saved-skin-native-drag-drop';
import { useSavedSkinUploadWorkflow } from './use-saved-skin-upload-workflow';
import { NO_CAPE_VALUE, type SavedSkinRecord } from './types';

function looksLikeUnresolvedDefaultSkin(skin: SavedSkinRecord, defaultKeyLookupComplete: boolean): boolean {
  if (defaultKeyLookupComplete || skin.source !== 'local_upload') return false;
  return DEFAULT_SKINS.some((defaultSkin) => defaultSkin.name === skin.name && defaultSkin.variant === skin.variant);
}

export function SavedSkinLibrary({
  skinActionDisabledReason,
  playerName,
  onRenameNametag,
}: {
  skinActionDisabledReason: string;
  playerName: string;
  onRenameNametag?: () => void;
}): JSX.Element {
  const dropSurfaceRef = useRef<HTMLElement | null>(null);
  const data = wardrobeData.value;
  const selection = wardrobeSelection.value;
  const op = wardrobeOp.value;
  const notice = wardrobeNotice.value;
  const { skinActionsEnabled, profile } = wardrobeContext.value;

  const lookup = useSavedSkinLookupWorkflow();
  const edit = useSavedSkinEditWorkflow();
  const uploadWorkflow = useSavedSkinUploadWorkflow();

  const skins = data.skins;
  const pendingApplyKey = data.pendingApplyKey;
  const profileSkin = activeMinecraftSkin(profile ?? undefined);
  const profileCape = activeMinecraftCape(profile ?? undefined);
  const availableCapes = profile?.capes ?? [];
  const capeById = useMemo(() => new Map(availableCapes.map((cape) => [cape.id, cape])), [availableCapes]);
  const capeSrcForId = (capeId: string | null | undefined): string | undefined => {
    if (!capeId) return undefined;
    const cape = capeById.get(capeId);
    return cape ? capeFileUrl(cape) : undefined;
  };
  const profileSkinVariant = skinVariantValue(profileSkin?.variant);
  const profileSkinFileSrc = profileSkin ? apiResourceUrl('/skin/profile/file') : undefined;
  const profileSkinIdentity =
    profileSkin && profile ? `profile:${profile.id}:${profileSkin.id}:${profileSkin.url}` : undefined;

  const sortedSkins = useMemo(() => sortSavedSkins(skins, 'recent'), [skins]);
  const savedSkinByKey = useMemo(() => new Map(skins.map((skin) => [skin.texture_key, skin])), [skins]);
  const keysById = defaultSkinKeys.value;
  const defaultKeysComplete = defaultSkinKeysReady.value;
  const defaultIdByKey = useMemo(() => {
    const ids = new Map<string, string>();
    for (const [id, key] of keysById) ids.set(key, id);
    return ids;
  }, [keysById]);
  const librarySkins = useMemo(
    () =>
      sortedSkins.filter(
        (skin) =>
          skin.source !== DEFAULT_SKIN_SOURCE &&
          !defaultIdByKey.has(skin.texture_key) &&
          !looksLikeUnresolvedDefaultSkin(skin, defaultKeysComplete),
      ),
    [sortedSkins, defaultIdByKey, defaultKeysComplete],
  );
  const savedRecordForDefault = (id: string): SavedSkinRecord | null => {
    const key = keysById.get(id);
    if (key) return savedSkinByKey.get(key) ?? null;
    const defaultSkin = DEFAULT_SKINS.find((skin) => skin.id === id);
    if (!defaultSkin) return null;
    return (
      skins.find(
        (skin) =>
          skin.source === DEFAULT_SKIN_SOURCE && skin.name === defaultSkin.name && skin.variant === defaultSkin.variant,
      ) ?? null
    );
  };

  const currentProfileSavedKey = inferredProfileSavedSkin()?.texture_key ?? null;
  const profileSavedRecord = currentProfileSavedKey ? (savedSkinByKey.get(currentProfileSavedKey) ?? null) : null;
  const equippedSkin = skins.find((skin) => Boolean(skin.applied_at)) ?? null;
  const pendingApplySkin = pendingApplyKey ? (savedSkinByKey.get(pendingApplyKey) ?? null) : null;
  const selectedSavedSkin = selection.kind === 'saved' ? (savedSkinByKey.get(selection.key) ?? null) : null;
  const selectedDefault =
    selection.kind === 'default' ? (DEFAULT_SKINS.find((skin) => skin.id === selection.id) ?? null) : null;
  const selectedSkinRecord =
    selection.kind === 'default'
      ? null
      : (selectedSavedSkin ?? profileSavedRecord ?? pendingApplySkin ?? equippedSkin ?? sortedSkins[0] ?? null);

  const lookupPreview = selection.kind === 'lookup' && lookup.lookupState === 'ready' ? lookup.lookupProfile : null;
  const profilePreviewActive = Boolean(selection.kind === 'profile' && profileSkin && profile);
  const showProfileSelectedPreview = Boolean(data.state === 'ready' && skins.length === 0 && profileSkin && profile);

  const stageNametag = local.hideSkinNametag ? null : playerName.trim() || null;
  const wornKey = (skin: SavedSkinRecord): boolean =>
    Boolean(skin.applied_at || (skinActionsEnabled && skin.texture_key === currentProfileSavedKey));

  const stageModel: SkinStageModel = lookupPreview
    ? { kind: 'lookup', profile: lookupPreview, variant: lookup.lookupVariant }
    : selectedDefault
      ? { kind: 'default', skin: selectedDefault }
      : profilePreviewActive
        ? { kind: 'profile', returnable: Boolean(selectedSkinRecord) }
        : selectedSkinRecord
          ? {
              kind: 'saved',
              skin: selectedSkinRecord,
              capeSrc: capeSrcForId(selectedSkinRecord.cape_id),
              queued: selectedSkinRecord.texture_key === pendingApplyKey,
              worn: wornKey(selectedSkinRecord),
              editing: edit.editKey === selectedSkinRecord.texture_key,
              editingSrc:
                edit.editKey === selectedSkinRecord.texture_key && edit.editReplacement
                  ? stagedSkinPreviewSrc(edit.editReplacement)
                  : null,
              editVariant: edit.editVariant,
              editCapeSrc: capeSrcForId(edit.editCapeId === NO_CAPE_VALUE ? null : edit.editCapeId),
            }
          : showProfileSelectedPreview
            ? { kind: 'profile', returnable: false }
            : data.state === 'ready'
              ? { kind: 'default', skin: DEFAULT_SKINS[0] }
              : { kind: 'empty', loading: data.state === 'loading' };

  const viewSavedSkin = async (textureKey: string): Promise<void> => {
    if (edit.editKey && edit.editKey !== textureKey) {
      const ok = await edit.closeSkinEditBeforeChanging();
      if (!ok) return;
    }
    selectSavedSkin(textureKey);
  };

  const applySkinGuarded = async (textureKey: string): Promise<void> => {
    const ok = await edit.closeSkinEditBeforeChanging();
    if (!ok) return;
    await applySkin(textureKey);
  };

  const startEditGuarded = async (skin: SavedSkinRecord): Promise<void> => {
    const started = await edit.startEdit(skin);
    if (!started) return;
    selectSavedSkin(skin.texture_key);
  };

  useSavedSkinNativeDragDrop({
    dropSurfaceRef,
    editKey: edit.editKey,
    uploadBusy: op !== null,
    editBusy: Boolean(edit.editBusyKey || edit.editDetectBusyKey),
    setUploadDragActive: uploadWorkflow.setUploadDragActive,
    setEditReplacementDragActive: edit.setEditReplacementDragActive,
    notifyError: setWardrobeNotice,
    onReadError: (err) => {
      setWardrobeNotice(wardrobeErrorMessage(err, 'Could not read dropped skin file.'));
    },
    stageUploadFile: uploadWorkflow.stageUploadFile,
    stageEditReplacementFile: edit.stageEditReplacementFile,
  });

  const profileMenuItems: ContextMenuItem[] = [
    ...(skinActionsEnabled && profileSkin && op === null
      ? [{ icon: 'download', label: 'Save locally', onSelect: () => void saveProfileSkinLocally() }]
      : []),
    ...(skinActionsEnabled && profileSkin && op === null
      ? [{ icon: 'x', label: 'Reset profile skin', onSelect: () => void resetProfileSkin() }]
      : []),
    ...(skinActionsEnabled && profileCape && op === null
      ? [{ icon: 'x', label: 'Reset profile cape', onSelect: () => void resetProfileCape() }]
      : []),
  ];
  const showProfileSkinTile = Boolean(profile && profileSkin && !showProfileSelectedPreview && !currentProfileSavedKey);

  const editingSkin = edit.editKey ? (savedSkinByKey.get(edit.editKey) ?? null) : null;
  const selectedSkinTextureKey = selectedSkinRecord?.texture_key ?? null;
  const previewExtraActive =
    selection.kind === 'default' || selection.kind === 'profile' || selection.kind === 'lookup';
  const stagedCapeSelected = uploadWorkflow.stagedCapeId !== NO_CAPE_VALUE;
  const stagedCapeSrc = capeSrcForId(stagedCapeSelected ? uploadWorkflow.stagedCapeId : null);
  const editPreviewCapeSrc = capeSrcForId(edit.editCapeId === NO_CAPE_VALUE ? null : edit.editCapeId);

  const inlineErrors = [
    lookup.lookupUsernameError && lookup.lookupState !== 'error' ? lookup.lookupUsernameError : null,
    lookup.lookupState === 'error' ? lookup.lookupError : null,
    notice,
    data.state === 'unavailable' ? (data.error ?? 'Saved skins are unavailable.') : null,
  ].filter((text): text is string => Boolean(text));

  const tileMenuItems = (skin: SavedSkinRecord): ContextMenuItem[] =>
    menuItemsForSavedSkin({
      skin,
      applied: wornKey(skin),
      selectedPreviewEditing: edit.editKey === skin.texture_key,
      skinActionsEnabled,
      applying: op?.kind === 'apply' || op?.kind === 'flush' || op?.kind === 'cancel-pending',
      pendingActionBusy: op?.kind === 'apply' || op?.kind === 'flush' || op?.kind === 'cancel-pending',
      queued: pendingApplyKey === skin.texture_key,
      deleting: op?.kind === 'delete' && op.key === skin.texture_key,
      onView: () => void viewSavedSkin(skin.texture_key),
      onApply: () => void applySkinGuarded(skin.texture_key),
      onApplyNow: () => void flushPendingApply(),
      onCancelQueue: () => void cancelPendingApply(),
      onEdit: () => void startEditGuarded(skin),
      onDownload: () => void downloadSavedSkin(skin),
      onDelete: () => void deleteSavedSkin(skin),
    });

  return (
    <>
      <SavedSkinFileInputs
        editTextureInputRef={edit.editTextureInputRef}
        fileInputRef={uploadWorkflow.fileInputRef}
        onEditTextureFile={edit.stageEditReplacementFile}
        onUploadFile={uploadWorkflow.handleUploadInputFile}
      />

      <SkinStage
        model={stageModel}
        nametag={stageNametag}
        onRenameNametag={onRenameNametag}
        onSaveLookup={(applyAfterSave) => void lookup.saveUsernameSkin(applyAfterSave)}
        onDismissLookup={lookup.dismissLookup}
        onStartEdit={startEditGuarded}
        onOpenUploadPicker={() => uploadWorkflow.openUploadPicker(false)}
      />

      <section
        ref={dropSurfaceRef}
        class="cp-skinhall__work"
        data-saved-skins-drop-surface
        data-saved-skins-drop-state={uploadWorkflow.uploadDragActive ? 'active' : 'idle'}
        onDragEnter={uploadWorkflow.uploadDrop.onDragEnter}
        onDragOver={uploadWorkflow.uploadDrop.onDragOver}
        onDragLeave={uploadWorkflow.uploadDrop.onDragLeave}
        onDrop={uploadWorkflow.uploadDrop.onDrop}
      >
        <header class="cp-skinhall__head">
          <div>
            <h1>Accounts &amp; skins</h1>
            <div class="cp-page-sub">Switch identities, preview and apply skins.</div>
          </div>
          <AccountSwitcherChip />
        </header>

        <SkinFinder
          username={lookup.lookupUsername}
          busy={lookup.lookupBusy && lookup.lookupState === 'loading'}
          canLookup={lookup.canLookupSkin}
          usernameError={lookup.lookupUsernameError}
          onUsernameChange={lookup.handleLookupUsernameChange}
          onLookup={() => void lookup.lookupSkin()}
        />

        <section class="cp-skin-section" aria-label="Skin library">
          <header class="cp-skin-section__head">
            <span class="cp-skin-section__title">Library</span>
            {data.state === 'ready' && librarySkins.length > 0 && (
              <span class="cp-skin-section__count">{librarySkins.length}</span>
            )}
          </header>
          {inlineErrors.length > 0 && (
            <div class="cp-skin-errors">
              {inlineErrors.map((text) => (
                <div key={text} class="cp-skin-inline-err">
                  {text}
                </div>
              ))}
            </div>
          )}
          {data.state === 'loading' ? (
            <div class="cp-skin-grid-note">Loading saved skins...</div>
          ) : (
            <SavedSkinLibraryGrid
              librarySkins={librarySkins}
              uploadDragActive={uploadWorkflow.uploadDragActive}
              canUpload={uploadWorkflow.canUpload}
              showProfileSkinTile={showProfileSkinTile}
              minecraftProfile={profile ?? undefined}
              profileSkin={profileSkin ?? undefined}
              profileSkinFileSrc={profileSkinFileSrc}
              profileSkinVariant={profileSkinVariant}
              profileCape={profileCape ?? undefined}
              profileSkinIdentity={profileSkinIdentity}
              profilePreviewActive={profilePreviewActive}
              profileMenuItems={profileMenuItems}
              selectedSkinTextureKey={selectedSkinTextureKey}
              previewExtraActive={previewExtraActive}
              skinActionsEnabled={skinActionsEnabled}
              currentProfileSavedKey={currentProfileSavedKey}
              pendingApplyKey={pendingApplyKey}
              deletingKey={op?.kind === 'delete' ? (op.key ?? null) : null}
              capeSrcForId={capeSrcForId}
              tileMenuItems={tileMenuItems}
              onOpenUploadPicker={() => uploadWorkflow.openUploadPicker(false)}
              onViewProfileSkin={previewProfileSkin}
              onViewSavedSkin={(textureKey) => void viewSavedSkin(textureKey)}
            />
          )}
        </section>

        <SavedSkinDefaultStrip
          skins={DEFAULT_SKINS}
          selectedDefaultId={selectedDefault?.id ?? null}
          selectedSkinTextureKey={selectedSkinTextureKey}
          previewExtraActive={previewExtraActive}
          pendingApplyKey={pendingApplyKey}
          skinActionsEnabled={skinActionsEnabled}
          currentProfileSavedKey={currentProfileSavedKey}
          savedRecordForDefault={savedRecordForDefault}
          onViewDefaultSkin={selectDefaultSkin}
        />

        {availableCapes.length > 0 && selectedSkinRecord && (
          <SavedSkinCapeSection
            availableCapes={availableCapes}
            selectedSkin={selectedSkinRecord}
            capeBusy={op?.kind === 'cape'}
            onChange={(value) => void changeSavedSkinCape(selectedSkinRecord, value === NO_CAPE_VALUE ? null : value)}
          />
        )}
      </section>

      <SkinUploadDialog
        stagedUpload={uploadWorkflow.stagedUpload}
        stagedVariant={uploadWorkflow.stagedVariant}
        stagedName={uploadWorkflow.stagedName}
        stagedCapeSrc={stagedCapeSrc}
        stagedCapeId={uploadWorkflow.stagedCapeId}
        availableCapes={availableCapes}
        skinName={uploadWorkflow.skinName}
        uploadVariant={uploadWorkflow.uploadVariant}
        busy={uploadWorkflow.busy}
        skinActionsEnabled={skinActionsEnabled}
        skinActionDisabledReason={skinActionDisabledReason}
        stagedCanSave={uploadWorkflow.stagedCanSave}
        onClose={uploadWorkflow.clearStagedUpload}
        onSkinNameChange={(value) => {
          uploadWorkflow.setSkinName(value);
          setWardrobeNotice(null);
        }}
        onUploadVariantChange={(value) => {
          uploadWorkflow.setUploadVariant(value);
          setWardrobeNotice(null);
        }}
        onStagedCapeChange={uploadWorkflow.setStagedCapeId}
        onSave={uploadWorkflow.saveStagedUpload}
      />

      <SkinEditDialog
        editingSkin={editingSkin}
        editReplacement={edit.editReplacement}
        editReplacementDragActive={edit.editReplacementDragActive}
        editPreviewCapeSrc={editPreviewCapeSrc}
        editName={edit.editName}
        trimmedEditName={edit.trimmedEditName}
        editVariant={edit.editVariant}
        editCapeId={edit.editCapeId}
        availableCapes={availableCapes}
        editBusyKey={edit.editBusyKey}
        editDetectBusyKey={edit.editDetectBusyKey}
        editDetectError={edit.editDetectError}
        editReplacementReady={edit.editReplacementReady}
        editHasChanges={editingSkin ? edit.savedSkinEditHasChanges(editingSkin) : false}
        skinActionsEnabled={skinActionsEnabled}
        skinActionDisabledReason={skinActionDisabledReason}
        deleteKey={op?.kind === 'delete' ? (op.key ?? null) : null}
        onClose={edit.cancelEdit}
        onEditReplacementDragEnter={edit.editReplacementDrop.onDragEnter}
        onEditReplacementDragOver={edit.editReplacementDrop.onDragOver}
        onEditReplacementDragLeave={edit.editReplacementDrop.onDragLeave}
        onEditReplacementDrop={edit.editReplacementDrop.onDrop}
        onEditNameChange={edit.setEditName}
        onEditVariantChange={(value) => {
          edit.setEditVariant(value);
          edit.setEditDetectError(null);
        }}
        onEditCapeChange={edit.setEditCapeId}
        onDetectModel={(skin) => void edit.detectSavedSkinModel(skin)}
        onOpenTexturePicker={edit.openEditTexturePicker}
        onClearEditReplacement={edit.clearEditReplacement}
        onDelete={(skin) => void deleteSavedSkin(skin)}
        onSave={(textureKey, applyAfterSave) => void edit.saveSkinMetadata(textureKey, applyAfterSave)}
      />
    </>
  );
}
