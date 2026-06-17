import type { JSX } from 'preact';
import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { api, apiResourceUrl } from '../../api';
import type { ContextMenuItem } from '../../ui/ContextMenu';
import { showConfirm } from '../../ui/Dialog';
import {
  hasSelectedSkinForAccount,
  resetSelectedSkin,
  selectedSkinForAccount,
  setSelectedSkin,
} from '../../player-skin';
import { local } from '../../state';
import { toast } from '../../toast';
import {
  activeMinecraftCape,
  activeMinecraftSkin,
  boundedMessage,
  capeFileUrl,
  commandSummary,
  defaultSkinFile,
  DEFAULT_SKIN_SOURCE,
  defaultSkinTextureKey,
  defaultSkinTextureKeys,
  downloadBlob,
  fetchSavedSkinPng,
  savedSkinDownloadFilename,
  savedSkinRecord,
  skinActionErrorMessage,
  skinFlushResult,
  skinVariantValue,
  sortSavedSkins,
  stagedSkinPreviewSrc,
} from './api';
import { DEFAULT_SKINS, type DefaultSkin } from '../../default-skins';
import { useSavedSkins } from './hooks';
import { SavedSkinCapeSection } from './SavedSkinCapeSection';
import { SavedSkinDefaultStrip } from './SavedSkinDefaultStrip';
import { SavedSkinFileInputs } from './SavedSkinFileInputs';
import { SavedSkinLibraryGrid } from './SavedSkinLibraryGrid';
import { SavedSkinLookupBar, type SavedSkinLibraryMessage } from './SavedSkinLookupBar';
import { SkinStage } from './SkinStage';
import { SkinEditDialog } from './SkinEditDialog';
import { SkinUploadDialog } from './SkinUploadDialog';
import { useSavedSkinEditWorkflow } from './use-saved-skin-edit-workflow';
import { useSavedSkinLookupWorkflow } from './use-saved-skin-lookup-workflow';
import { useSavedSkinNativeDragDrop } from './use-saved-skin-native-drag-drop';
import { useSavedSkinUploadWorkflow } from './use-saved-skin-upload-workflow';
import {
  NO_CAPE_VALUE,
  type AccountActionState,
  type MinecraftProfile,
  type SavedSkinRecord,
  type SkinVariant,
} from './types';

type StagePreviewExtra = { kind: 'default'; id: string } | { kind: 'profile' } | { kind: 'lookup' };

const PROFILE_SKIN_SOURCE = 'minecraft_profile_skin';
function looksLikeUnresolvedDefaultSkin(skin: SavedSkinRecord, defaultKeyLookupComplete: boolean): boolean {
  if (defaultKeyLookupComplete || skin.source !== 'local_upload') return false;
  return DEFAULT_SKINS.some((defaultSkin) => defaultSkin.name === skin.name && defaultSkin.variant === skin.variant);
}
export function SavedSkinLibrary({
  skinAction,
  minecraftProfile,
  skinAccountKey,
  playerName,
  onRenameNametag,
  onApplied,
}: {
  skinAction?: AccountActionState;
  minecraftProfile?: MinecraftProfile;
  skinAccountKey: string;
  playerName: string;
  onRenameNametag?: () => void;
  onApplied: () => void;
}): JSX.Element {
  const savedSkinsDropSurfaceRef = useRef<HTMLElement | null>(null);
  const {
    skins,
    pendingApplyKey,
    state,
    error,
    refresh,
    setPendingApplyKey: setLocalPendingApplyKey,
  } = useSavedSkins();
  const skinActionsEnabled = skinAction?.enabled === true;
  const skinActionDisabledReason =
    skinAction?.disabled_reason || skinAction?.detail || 'Online Minecraft account required';
  const [profileBusy, setProfileBusy] = useState(false);
  const [profileResetBusy, setProfileResetBusy] = useState(false);
  const [profileCapeResetBusy, setProfileCapeResetBusy] = useState(false);
  const [message, setMessage] = useState<SavedSkinLibraryMessage | null>(null);
  const [deleteKey, setDeleteKey] = useState<string | null>(null);
  const [applyKey, setApplyKey] = useState<string | null>(null);
  const [downloadKey, setDownloadKey] = useState<string | null>(null);
  const [flushBusy, setFlushBusy] = useState(false);
  const [cancelPendingBusy, setCancelPendingBusy] = useState(false);
  const [capeBusy, setCapeBusy] = useState(false);
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const [previewExtra, setPreviewExtra] = useState<StagePreviewExtra | null>(null);
  const [profileSavedKey, setProfileSavedKey] = useState<string | null>(null);
  const [defaultKeyById, setDefaultKeyById] = useState<Map<string, string>>(() => new Map());
  const [defaultKeyLookupComplete, setDefaultKeyLookupComplete] = useState(false);
  const {
    lookupUsername,
    lookupProfile,
    lookupState,
    lookupError,
    lookupVariant,
    lookupBusy,
    lookupUsernameError,
    canLookupSkin: lookupInputCanLookup,
    canSaveLookupSkin: lookupResultCanSave,
    lookupSkin,
    dismissLookup,
    saveUsernameSkin,
    handleLookupUsernameChange,
  } = useSavedSkinLookupWorkflow({
    skinAccountKey,
    setMessage,
    setSelectedKey,
    setPreviewExtra,
    refresh,
    applySavedSkin,
  });
  const {
    editTextureInputRef,
    editReplacement,
    editReplacementDragActive,
    editKey,
    editName,
    editVariant,
    editCapeId,
    editBusyKey,
    editDetectBusyKey,
    editDetectError,
    trimmedEditName,
    editReplacementReady,
    editReplacementDrop,
    setEditName,
    setEditVariant,
    setEditCapeId,
    setEditDetectError,
    setEditReplacementDragActive,
    clearEditReplacement,
    closeSkinEditBeforeChanging,
    startEdit,
    cancelEdit,
    detectSavedSkinModel,
    stageEditReplacementFile,
    openEditTexturePicker,
    saveSkinMetadata,
    savedSkinEditHasChanges,
  } = useSavedSkinEditWorkflow({
    skins,
    skinActionsEnabled,
    skinAccountKey,
    setMessage,
    setSelectedKey,
    refresh,
    applySavedSkin,
  });
  const {
    fileInputRef,
    stagedUpload,
    stagedVariant,
    stagedName,
    stagedCapeId,
    skinName,
    uploadVariant,
    uploadDragActive,
    busy,
    canUpload,
    stagedCanSave,
    uploadDrop,
    setSkinName,
    setUploadVariant,
    setStagedCapeId,
    setUploadDragActive,
    setUploadBusy,
    clearStagedUpload,
    stageUploadFile,
    openUploadPicker,
    saveStagedUpload,
    handleUploadInputFile,
    upload,
  } = useSavedSkinUploadWorkflow({
    skinActionsEnabled,
    profileBusy,
    profileResetBusy,
    profileCapeResetBusy,
    lookupBusy,
    skinAccountKey,
    setMessage,
    setSelectedKey,
    setPreviewExtra,
    refresh,
    applySavedSkin,
  });
  const profileSeedRef = useRef('');
  const profileSkin = activeMinecraftSkin(minecraftProfile);
  const profileCape = activeMinecraftCape(minecraftProfile);
  const availableCapes = minecraftProfile?.capes ?? [];
  const capeById = useMemo(() => new Map(availableCapes.map((cape) => [cape.id, cape])), [availableCapes]);
  const profileSkinVariant = skinVariantValue(profileSkin?.variant);
  const profileSkinFileSrc = profileSkin ? apiResourceUrl('/skin/profile/file') : undefined;
  const profileSkinIdentity =
    profileSkin && minecraftProfile ? `profile:${minecraftProfile.id}:${profileSkin.id}:${profileSkin.url}` : undefined;
  const canSaveProfileSkin =
    skinActionsEnabled &&
    Boolean(profileSkin) &&
    !busy &&
    !profileBusy &&
    !profileResetBusy &&
    !profileCapeResetBusy &&
    !lookupBusy;
  const canResetProfileSkin =
    skinActionsEnabled &&
    Boolean(profileSkin) &&
    !busy &&
    !profileBusy &&
    !profileResetBusy &&
    !profileCapeResetBusy &&
    !lookupBusy;
  const canResetProfileCape =
    skinActionsEnabled &&
    Boolean(profileCape) &&
    !busy &&
    !profileBusy &&
    !profileResetBusy &&
    !profileCapeResetBusy &&
    !lookupBusy;
  const canLookupSkin = lookupInputCanLookup && !busy && !profileBusy && !profileResetBusy && !profileCapeResetBusy;
  const canSaveLookupSkin = lookupResultCanSave && !busy && !profileBusy && !profileResetBusy && !profileCapeResetBusy;
  const equippedSkin = skins.find((skin) => Boolean(skin.applied_at)) ?? null;
  const pendingApplySkin = skins.find((skin) => skin.texture_key === pendingApplyKey) ?? null;
  const selectedSavedSkin = selectedKey ? (skins.find((skin) => skin.texture_key === selectedKey) ?? null) : null;
  const inferredProfileSavedSkin =
    minecraftProfile && profileSkin
      ? (skins.find(
          (skin) =>
            skin.source === PROFILE_SKIN_SOURCE &&
            skin.name === `${minecraftProfile.name.trim()} profile skin` &&
            skin.variant === profileSkinVariant &&
            (skin.cape_id ?? null) === (profileCape?.id ?? null),
        ) ??
        skins.find((skin) => skin.source === PROFILE_SKIN_SOURCE && Boolean(skin.applied_at)) ??
        null)
      : null;
  const profileSavedSkin =
    (profileSavedKey ? (skins.find((skin) => skin.texture_key === profileSavedKey) ?? null) : null) ??
    inferredProfileSavedSkin;
  const currentProfileSavedKey = profileSavedKey ?? profileSavedSkin?.texture_key ?? null;
  const selectedSkin =
    previewExtra?.kind === 'default'
      ? null
      : (selectedSavedSkin ?? profileSavedSkin ?? pendingApplySkin ?? equippedSkin ?? skins[0] ?? null);
  const sortedSkins = useMemo(() => sortSavedSkins(skins, 'recent'), [skins]);
  const savedSkinByKey = useMemo(() => new Map(skins.map((skin) => [skin.texture_key, skin])), [skins]);
  const defaultIdByKey = useMemo(() => {
    const ids = new Map<string, string>();
    for (const [id, key] of defaultKeyById) ids.set(key, id);
    return ids;
  }, [defaultKeyById]);
  const librarySkins = useMemo(
    () =>
      sortedSkins.filter(
        (skin) =>
          skin.source !== DEFAULT_SKIN_SOURCE &&
          !defaultIdByKey.has(skin.texture_key) &&
          !looksLikeUnresolvedDefaultSkin(skin, defaultKeyLookupComplete),
      ),
    [sortedSkins, defaultIdByKey, defaultKeyLookupComplete],
  );
  const savedRecordForDefault = (id: string): SavedSkinRecord | null => {
    const key = defaultKeyById.get(id);
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
  const selectedDefault =
    previewExtra?.kind === 'default' ? (DEFAULT_SKINS.find((skin) => skin.id === previewExtra.id) ?? null) : null;
  const selectedPreviewEditing = Boolean(selectedSkin && editKey === selectedSkin.texture_key);
  const selectedQueued = Boolean(selectedSkin && selectedSkin.texture_key === pendingApplyKey);
  const showProfileSelectedPreview = Boolean(
    state === 'ready' && skins.length === 0 && profileSkin && minecraftProfile,
  );
  const capeSrcForId = (capeId: string | null | undefined): string | undefined => {
    if (!capeId) return undefined;
    const cape = capeById.get(capeId);
    return cape ? capeFileUrl(cape) : undefined;
  };
  const stagedCapeSelected = stagedCapeId !== NO_CAPE_VALUE;
  const stagedCapeSrc = capeSrcForId(stagedCapeSelected ? stagedCapeId : null);
  useEffect(() => {
    const selectedPreference = selectedSkinForAccount(skinAccountKey);
    if (skinActionsEnabled && profileSkin && !hasSelectedSkinForAccount(skinAccountKey)) {
      setSelectedKey(null);
      setPreviewExtra({ kind: 'profile' });
      return;
    }
    if (selectedPreference.startsWith('saved:')) {
      const textureKey = selectedPreference.slice('saved:'.length);
      setSelectedKey(textureKey || null);
      setPreviewExtra(null);
      return;
    }
    if (!selectedPreference.startsWith('default:')) return;
    const id = selectedPreference.slice('default:'.length);
    if (DEFAULT_SKINS.some((skin) => skin.id === id)) {
      setSelectedKey(null);
      setPreviewExtra({ kind: 'default', id });
    }
  }, [skinActionsEnabled, profileSkin, skinAccountKey]);

  useEffect(() => {
    let active = true;
    void defaultSkinTextureKeys()
      .then((keys) => {
        if (active) {
          setDefaultKeyById(keys);
          setDefaultKeyLookupComplete(true);
        }
      })
      .catch(() => {});
    return () => {
      active = false;
    };
  }, []);

  useEffect(() => {
    if (state !== 'ready' || !skinActionsEnabled || !minecraftProfile || !profileSkin || !profileSkinIdentity) {
      setProfileSavedKey(null);
      return;
    }

    const seedId = `${skinAccountKey}:${profileSkinIdentity}`;
    if (profileSeedRef.current === seedId) return;
    profileSeedRef.current = seedId;

    let active = true;
    void api('POST', '/skins/from-profile', { variant: profileSkinVariant, mark_current: true })
      .then((payload) => {
        if (!active) return;
        const saved = savedSkinRecord(payload);
        if (!saved) return;
        setProfileSavedKey(saved.texture_key);
        if (!hasSelectedSkinForAccount(skinAccountKey)) {
          setSelectedKey(saved.texture_key);
          setPreviewExtra(null);
          setSelectedSkin(`saved:${saved.texture_key}`, skinAccountKey);
        }
        refresh();
      })
      .catch(() => {
        if (active && profileSeedRef.current === seedId) profileSeedRef.current = '';
      });

    return () => {
      active = false;
    };
  }, [
    minecraftProfile,
    skinActionsEnabled,
    profileSkin,
    profileSkinIdentity,
    profileSkinVariant,
    refresh,
    skinAccountKey,
    state,
  ]);

  useEffect(() => {
    if (state !== 'ready') return;
    const selectedPreference = selectedSkinForAccount(skinAccountKey);
    if (skinActionsEnabled && profileSkin && !hasSelectedSkinForAccount(skinAccountKey)) {
      return;
    }
    if (skins.length === 0) {
      if (selectedKey !== null) setSelectedKey(null);
      if (!selectedPreference.startsWith('default:')) resetSelectedSkin(skinAccountKey);
      return;
    }
    if (selectedKey && skins.some((skin) => skin.texture_key === selectedKey)) return;
    if (selectedPreference.startsWith('default:')) return;
    const selectedSavedKey = selectedPreference.startsWith('saved:') ? selectedPreference.slice('saved:'.length) : null;
    const next =
      (selectedSavedKey ? skins.find((skin) => skin.texture_key === selectedSavedKey) : undefined) ??
      skins.find((skin) => Boolean(skin.applied_at)) ??
      skins[0];
    setSelectedKey(next.texture_key);
    setSelectedSkin(`saved:${next.texture_key}`, skinAccountKey);
  }, [skinActionsEnabled, profileSkin, skinAccountKey, skins, selectedKey, state]);

  useEffect(() => {
    if (!pendingApplyKey) return;
    const timer = window.setTimeout(() => {
      refresh();
      onApplied();
    }, 11_500);
    return () => window.clearTimeout(timer);
  }, [onApplied, pendingApplyKey, refresh]);

  const deleteSkin = async (textureKey: string, skinName?: string): Promise<void> => {
    setDeleteKey(textureKey);
    setMessage(null);
    try {
      await api('DELETE', `/skins/${textureKey}`);
      if (selectedKey === textureKey) setSelectedKey(null);
      if (selectedSkinForAccount(skinAccountKey) === `saved:${textureKey}`) {
        resetSelectedSkin(skinAccountKey);
      }
      refresh();
      const deletedName = skinName?.trim();
      toast(deletedName ? `Deleted "${deletedName}"` : 'Skin deleted');
    } catch (err) {
      setMessage({
        tone: 'err',
        text: err instanceof Error ? err.message : 'Could not delete skin.',
      });
    } finally {
      setDeleteKey(null);
    }
  };

  const confirmDeleteSkin = async (skin: SavedSkinRecord): Promise<void> => {
    const name = skin.name.trim();
    const ok = await showConfirm(
      name
        ? `Delete saved skin "${name}"? This removes it from local saved skins only.`
        : 'Delete this saved skin? This removes it from local saved skins only.',
      { title: 'Delete saved skin', destructive: true, confirmText: 'Delete' },
    );
    if (!ok) return;
    await deleteSkin(skin.texture_key, skin.name);
  };

  const saveProfileSkin = async (): Promise<void> => {
    if (!skinActionsEnabled) return;

    setProfileBusy(true);
    setMessage(null);
    try {
      const request: { variant?: SkinVariant; mark_current: true } = { mark_current: true };
      if (profileSkin) request.variant = profileSkinVariant;
      const payload = await api('POST', '/skins/from-profile', request);
      const saved = savedSkinRecord(payload);
      if (saved) {
        setSelectedKey(saved.texture_key);
        setPreviewExtra(null);
        setSelectedSkin(`saved:${saved.texture_key}`, skinAccountKey);
      }
      refresh();
      toast('Profile skin added to your library');
    } catch (err) {
      setMessage({
        tone: 'err',
        text: skinActionErrorMessage(err, 'Could not save Minecraft profile skin.'),
      });
    } finally {
      setProfileBusy(false);
    }
  };

  const resetProfileSkin = async (): Promise<void> => {
    if (!skinActionsEnabled || !profileSkin) return;
    const ok = await showConfirm(
      'Reset the active Minecraft profile skin to the default skin? Croopor will save the current profile skin locally first.',
      { title: 'Reset profile skin', destructive: true, confirmText: 'Reset' },
    );
    if (!ok) return;

    setProfileResetBusy(true);
    setMessage(null);
    try {
      const response = await api('POST', '/skin/profile/reset', {});
      refresh();
      onApplied();
      toast(commandSummary(response, 'Skin command accepted.'));
    } catch (err) {
      setMessage({
        tone: 'err',
        text: skinActionErrorMessage(err, 'Could not reset Minecraft profile skin.'),
      });
    } finally {
      setProfileResetBusy(false);
    }
  };

  const resetProfileCape = async (): Promise<void> => {
    if (!skinActionsEnabled || !profileCape) return;
    const ok = await showConfirm(
      'Remove the active Minecraft profile cape? Croopor will save the current skin and cape pairing locally first.',
      { title: 'Reset profile cape', destructive: true, confirmText: 'Reset cape' },
    );
    if (!ok) return;

    setProfileCapeResetBusy(true);
    setMessage(null);
    try {
      const response = await api('POST', '/skin/cape/reset', {});
      refresh();
      onApplied();
      toast(commandSummary(response, 'Skin command accepted.'));
    } catch (err) {
      setMessage({
        tone: 'err',
        text: skinActionErrorMessage(err, 'Could not reset Minecraft profile cape.'),
      });
    } finally {
      setProfileCapeResetBusy(false);
    }
  };

  async function applySavedSkin(textureKey: string, options: { select?: boolean } = {}): Promise<string> {
    const response = await api('POST', `/skins/${textureKey}/apply?defer=true`);
    setLocalPendingApplyKey(textureKey);
    if (options.select !== false) {
      setSelectedKey(textureKey);
      setSelectedSkin(`saved:${textureKey}`, skinAccountKey);
    }
    refresh();
    return commandSummary(response, 'Skin command accepted.');
  }

  const viewSavedSkin = async (textureKey: string): Promise<void> => {
    if (editKey && editKey !== textureKey) {
      const ok = await closeSkinEditBeforeChanging();
      if (!ok) return;
    }

    setSelectedKey(textureKey);
    setPreviewExtra(null);
    setMessage(null);
    setSelectedSkin(`saved:${textureKey}`, skinAccountKey);
  };

  const viewDefaultSkin = (id: string): void => {
    setSelectedKey(null);
    setPreviewExtra({ kind: 'default', id });
    setMessage(null);
    setSelectedSkin(`default:${id}`, skinAccountKey);
  };

  const viewProfileSkin = (): void => {
    setPreviewExtra({ kind: 'profile' });
    setMessage(null);
  };

  const applyDefaultSkin = async (skin: DefaultSkin): Promise<void> => {
    setSelectedKey(null);
    setPreviewExtra({ kind: 'default', id: skin.id });
    setSelectedSkin(`default:${skin.id}`, skinAccountKey);
    const existingKey = await defaultSkinTextureKey(skin).catch(() => defaultKeyById.get(skin.id));
    if (existingKey && defaultKeyById.get(skin.id) !== existingKey) {
      setDefaultKeyById((current) => {
        if (current.get(skin.id) === existingKey) return current;
        const next = new Map(current);
        next.set(skin.id, existingKey);
        return next;
      });
    }
    const existing = existingKey ? (savedSkinByKey.get(existingKey) ?? null) : null;
    if (existing) {
      setUploadBusy(true);
      setMessage(null);
      try {
        toast(await applySavedSkin(existing.texture_key, { select: false }));
      } catch (err) {
        setMessage({
          tone: 'err',
          text: skinActionErrorMessage(err, 'Could not apply skin.'),
        });
      } finally {
        setUploadBusy(false);
      }
      return;
    }
    await upload(await defaultSkinFile(skin), true, skin.variant, NO_CAPE_VALUE, DEFAULT_SKIN_SOURCE);
    setSelectedKey(null);
    setPreviewExtra({ kind: 'default', id: skin.id });
    setSelectedSkin(`default:${skin.id}`, skinAccountKey);
  };

  const applySkin = async (textureKey: string): Promise<void> => {
    const skin = skins.find((saved) => saved.texture_key === textureKey);
    if (skin?.applied_at) return;
    const ok = await closeSkinEditBeforeChanging();
    if (!ok) return;

    setApplyKey(textureKey);
    setMessage(null);
    try {
      toast(await applySavedSkin(textureKey));
    } catch (err) {
      setMessage({
        tone: 'err',
        text: skinActionErrorMessage(err, 'Could not apply skin.'),
      });
    } finally {
      setApplyKey(null);
    }
  };

  const changeSelectedSkinCape = async (value: string): Promise<void> => {
    if (!selectedSkin || capeBusy) return;
    const nextCapeId = value === NO_CAPE_VALUE ? null : value;
    if ((selectedSkin.cape_id ?? null) === nextCapeId) return;

    setPreviewExtra(null);
    setCapeBusy(true);
    setMessage(null);
    try {
      const updated = savedSkinRecord(
        await api('PUT', `/skins/${selectedSkin.texture_key}`, {
          name: selectedSkin.name,
          variant: selectedSkin.variant,
          cape_id: nextCapeId,
        }),
      );
      if (!updated) throw new Error('Cape update returned an invalid response.');
      if (selectedSkin.applied_at && skinActionsEnabled) {
        toast(await applySavedSkin(updated.texture_key));
      } else {
        refresh();
        toast('Cape updated');
      }
    } catch (err) {
      setMessage({
        tone: 'err',
        text: skinActionErrorMessage(err, 'Could not update the cape.'),
      });
    } finally {
      setCapeBusy(false);
    }
  };

  const flushPendingApply = async (): Promise<void> => {
    setFlushBusy(true);
    setMessage(null);
    try {
      const payload = await api('POST', '/skins/flush');
      const result = skinFlushResult(payload);
      if (!result) throw new Error('Skin flush returned an invalid response.');
      if (result.applied > 0) {
        onApplied();
        setLocalPendingApplyKey(null);
      } else {
        setLocalPendingApplyKey(null);
      }
      toast(result.viewModel?.summary ?? 'Skin command accepted.');
      refresh();
    } catch (err) {
      setMessage({
        tone: 'err',
        text: skinActionErrorMessage(err, 'Could not apply queued skin.'),
      });
    } finally {
      setFlushBusy(false);
    }
  };

  const cancelPendingApply = async (): Promise<void> => {
    setCancelPendingBusy(true);
    setMessage(null);
    try {
      const response = await api('DELETE', '/skins/pending');
      setLocalPendingApplyKey(null);
      refresh();
      toast(commandSummary(response, 'Skin change canceled.'));
    } catch (err) {
      setMessage({
        tone: 'err',
        text: err instanceof Error ? err.message : 'Could not cancel queued skin apply.',
      });
    } finally {
      setCancelPendingBusy(false);
    }
  };

  const downloadSavedSkin = async (skin: SavedSkinRecord): Promise<void> => {
    if (downloadKey === skin.texture_key) return;
    setDownloadKey(skin.texture_key);
    setMessage(null);
    try {
      const blob = await fetchSavedSkinPng(skin);
      downloadBlob(blob, savedSkinDownloadFilename(skin));
      toast('Skin PNG downloaded');
    } catch (err) {
      setMessage({
        tone: 'err',
        text: boundedMessage(err instanceof Error ? err.message : undefined, 'Could not download skin PNG.'),
      });
    } finally {
      setDownloadKey(null);
    }
  };

  const resetPreview = (): void => {
    resetSelectedSkin(skinAccountKey);
    setSelectedKey(null);
    setPreviewExtra({ kind: 'default', id: 'steve' });
    setMessage(null);
  };

  useSavedSkinNativeDragDrop({
    dropSurfaceRef: savedSkinsDropSurfaceRef,
    editKey,
    uploadBusy: busy || profileBusy || profileResetBusy || profileCapeResetBusy || lookupBusy,
    editBusy: Boolean(editBusyKey || editDetectBusyKey),
    setUploadDragActive,
    setEditReplacementDragActive,
    setMessage,
    onReadError: (err) => {
      setMessage({
        tone: 'err',
        text: boundedMessage(err instanceof Error ? err.message : undefined, 'Could not read dropped skin file.'),
      });
    },
    stageUploadFile,
    stageEditReplacementFile,
  });

  const editingSkin = editKey ? (skins.find((skin) => skin.texture_key === editKey) ?? null) : null;
  const lookupPreview = previewExtra?.kind === 'lookup' && lookupState === 'ready' ? lookupProfile : null;
  const profilePreviewActive = Boolean(previewExtra?.kind === 'profile' && profileSkin && minecraftProfile);
  const stageDefaultSkin =
    selectedDefault ?? (state === 'ready' && !selectedSkin && !profileSkin ? DEFAULT_SKINS[0] : null);
  const stageNametag = local.hideSkinNametag ? null : playerName.trim() || null;
  const stageEditingSrc = selectedPreviewEditing && editReplacement ? stagedSkinPreviewSrc(editReplacement) : null;
  const editPreviewCapeSrc = capeSrcForId(editCapeId === NO_CAPE_VALUE ? null : editCapeId);
  const stageApplyBusy = Boolean(selectedSkin && applyKey === selectedSkin.texture_key);
  const selectedSkinForStage =
    selectedSkin && skinActionsEnabled && selectedSkin.texture_key === currentProfileSavedKey
      ? { ...selectedSkin, applied_at: selectedSkin.applied_at ?? 'minecraft_profile' }
      : selectedSkin;
  const profileMenuItems: ContextMenuItem[] = [
    ...(canSaveProfileSkin
      ? [{ icon: 'download', label: 'Save locally', onSelect: () => void saveProfileSkin() }]
      : []),
    ...(canResetProfileSkin
      ? [{ icon: 'x', label: 'Reset profile skin', onSelect: () => void resetProfileSkin() }]
      : []),
    ...(canResetProfileCape
      ? [{ icon: 'x', label: 'Reset profile cape', onSelect: () => void resetProfileCape() }]
      : []),
  ];
  const showProfileSkinTile = Boolean(
    minecraftProfile && profileSkin && !showProfileSelectedPreview && !currentProfileSavedKey,
  );
  const previewExtraActive = Boolean(previewExtra);
  const selectedSkinTextureKey = selectedSkin?.texture_key ?? null;

  return (
    <div class="cp-skinpage">
      <SavedSkinFileInputs
        editTextureInputRef={editTextureInputRef}
        fileInputRef={fileInputRef}
        onEditTextureFile={stageEditReplacementFile}
        onUploadFile={handleUploadInputFile}
      />

      <SkinStage
        state={state}
        skinActionsEnabled={skinActionsEnabled}
        lookupPreview={lookupPreview}
        lookupVariant={lookupVariant}
        lookupBusy={lookupBusy}
        canSaveLookupSkin={canSaveLookupSkin}
        onDismissLookup={dismissLookup}
        onSaveUsernameSkin={(applyAfterSave) => void saveUsernameSkin(applyAfterSave)}
        stageDefaultSkin={stageDefaultSkin}
        selectedDefault={selectedDefault}
        busy={busy}
        canUpload={canUpload}
        stageNametag={stageNametag}
        onRenameNametag={onRenameNametag}
        onResetPreview={resetPreview}
        onApplyDefaultSkin={(skin) => void applyDefaultSkin(skin)}
        profilePreviewActive={profilePreviewActive}
        showProfileSelectedPreview={showProfileSelectedPreview}
        minecraftProfile={minecraftProfile}
        profileSkin={profileSkin}
        profileCape={profileCape}
        profileSkinFileSrc={profileSkinFileSrc}
        profileSkinVariant={profileSkinVariant}
        profileBusy={profileBusy}
        canSaveProfileSkin={canSaveProfileSkin}
        selectedSkin={selectedSkinForStage}
        selectedSkinCapeSrc={selectedSkinForStage ? capeSrcForId(selectedSkinForStage.cape_id) : undefined}
        selectedQueued={selectedQueued}
        selectedPreviewEditing={selectedPreviewEditing}
        stageEditingSrc={stageEditingSrc}
        editPreviewCapeSrc={editPreviewCapeSrc}
        editVariant={editVariant}
        stageApplyBusy={stageApplyBusy}
        cancelPendingBusy={cancelPendingBusy}
        flushBusy={flushBusy}
        applyKey={applyKey}
        deleteKey={deleteKey}
        onReturnFromProfile={() => setPreviewExtra(null)}
        onSaveProfileSkin={() => void saveProfileSkin()}
        onCancelPendingApply={() => void cancelPendingApply()}
        onFlushPendingApply={() => void flushPendingApply()}
        onApplySkin={(textureKey) => void applySkin(textureKey)}
        onStartEdit={(skin) => void startEdit(skin)}
        onOpenUploadPicker={() => openUploadPicker(false)}
      />

      <section
        ref={savedSkinsDropSurfaceRef}
        class="cp-skinwork"
        data-saved-skins-drop-surface
        data-saved-skins-drop-state={uploadDragActive ? 'active' : 'idle'}
        onDragEnter={uploadDrop.onDragEnter}
        onDragOver={uploadDrop.onDragOver}
        onDragLeave={uploadDrop.onDragLeave}
        onDrop={uploadDrop.onDrop}
      >
        <SavedSkinLookupBar
          lookupUsername={lookupUsername}
          lookupBusy={lookupBusy}
          lookupState={lookupState}
          lookupUsernameError={lookupUsernameError}
          lookupError={lookupError}
          message={message}
          state={state}
          error={error}
          canLookupSkin={canLookupSkin}
          onLookupUsernameChange={handleLookupUsernameChange}
          onLookupSkin={() => void lookupSkin()}
        />

        <SavedSkinLibraryGrid
          state={state}
          librarySkins={librarySkins}
          uploadDragActive={uploadDragActive}
          canUpload={canUpload}
          showProfileSkinTile={showProfileSkinTile}
          minecraftProfile={minecraftProfile}
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
          deleteKey={deleteKey}
          editKey={editKey}
          applyKey={applyKey}
          flushBusy={flushBusy}
          cancelPendingBusy={cancelPendingBusy}
          capeSrcForId={capeSrcForId}
          onOpenUploadPicker={() => openUploadPicker(false)}
          onViewProfileSkin={viewProfileSkin}
          onViewSavedSkin={(textureKey) => void viewSavedSkin(textureKey)}
          onApplySkin={(textureKey) => void applySkin(textureKey)}
          onFlushPendingApply={() => void flushPendingApply()}
          onCancelPendingApply={() => void cancelPendingApply()}
          onStartEdit={(skin) => void startEdit(skin)}
          onDownloadSavedSkin={(skin) => void downloadSavedSkin(skin)}
          onConfirmDeleteSkin={(skin) => void confirmDeleteSkin(skin)}
        />

        <SavedSkinDefaultStrip
          skins={DEFAULT_SKINS}
          selectedDefaultId={selectedDefault?.id ?? null}
          selectedSkinTextureKey={selectedSkinTextureKey}
          previewExtraActive={previewExtraActive}
          pendingApplyKey={pendingApplyKey}
          skinActionsEnabled={skinActionsEnabled}
          currentProfileSavedKey={currentProfileSavedKey}
          savedRecordForDefault={savedRecordForDefault}
          onViewDefaultSkin={viewDefaultSkin}
        />

        {availableCapes.length > 0 && selectedSkin && (
          <SavedSkinCapeSection
            availableCapes={availableCapes}
            selectedSkin={selectedSkin}
            capeBusy={capeBusy}
            onChange={(value) => void changeSelectedSkinCape(value)}
          />
        )}
      </section>

      <SkinUploadDialog
        stagedUpload={stagedUpload}
        stagedVariant={stagedVariant}
        stagedName={stagedName}
        stagedCapeSrc={stagedCapeSrc}
        stagedCapeId={stagedCapeId}
        availableCapes={availableCapes}
        skinName={skinName}
        uploadVariant={uploadVariant}
        busy={busy}
        skinActionsEnabled={skinActionsEnabled}
        skinActionDisabledReason={skinActionDisabledReason}
        stagedCanSave={stagedCanSave}
        onClose={clearStagedUpload}
        onSkinNameChange={(value) => {
          setSkinName(value);
          setMessage(null);
        }}
        onUploadVariantChange={(value) => {
          setUploadVariant(value);
          setMessage(null);
        }}
        onStagedCapeChange={setStagedCapeId}
        onSave={saveStagedUpload}
      />

      <SkinEditDialog
        editingSkin={editingSkin}
        editReplacement={editReplacement}
        editReplacementDragActive={editReplacementDragActive}
        editPreviewCapeSrc={editPreviewCapeSrc}
        editName={editName}
        trimmedEditName={trimmedEditName}
        editVariant={editVariant}
        editCapeId={editCapeId}
        availableCapes={availableCapes}
        editBusyKey={editBusyKey}
        editDetectBusyKey={editDetectBusyKey}
        editDetectError={editDetectError}
        editReplacementReady={editReplacementReady}
        editHasChanges={editingSkin ? savedSkinEditHasChanges(editingSkin) : false}
        skinActionsEnabled={skinActionsEnabled}
        skinActionDisabledReason={skinActionDisabledReason}
        deleteKey={deleteKey}
        onClose={cancelEdit}
        onEditReplacementDragEnter={editReplacementDrop.onDragEnter}
        onEditReplacementDragOver={editReplacementDrop.onDragOver}
        onEditReplacementDragLeave={editReplacementDrop.onDragLeave}
        onEditReplacementDrop={editReplacementDrop.onDrop}
        onEditNameChange={setEditName}
        onEditVariantChange={(value) => {
          setEditVariant(value);
          setEditDetectError(null);
        }}
        onEditCapeChange={setEditCapeId}
        onDetectModel={(skin) => void detectSavedSkinModel(skin)}
        onOpenTexturePicker={openEditTexturePicker}
        onClearEditReplacement={clearEditReplacement}
        onDelete={(skin) => void confirmDeleteSkin(skin)}
        onSave={(textureKey, applyAfterSave) => void saveSkinMetadata(textureKey, applyAfterSave)}
      />
    </div>
  );
}
