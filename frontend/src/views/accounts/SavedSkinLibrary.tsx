import type { JSX } from 'preact';
import { useCallback, useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { api, apiResourceUrl, apiUrl } from '../../api';
import { Button, IconButton, Input, Segmented } from '../../ui/Atoms';
import { openContextMenu, type ContextMenuItem } from '../../ui/ContextMenu';
import { showConfirm } from '../../ui/Dialog';
import { Icon } from '../../ui/Icons';
import { Modal, ModalContent, ModalHeader, ModalTitle } from '../../ui/Modal';
import { onNativeDragDrop, pickNativeSkinFile, readNativeSkinFile, type NativeDragDropPayload } from '../../native';
import { clampPlayerNameInput } from '../../player-name';
import { resetSelectedSkin, setSelectedSkin } from '../../player-skin';
import { local } from '../../state';
import { toast } from '../../toast';
import { validateUsername } from '../../utils';
import {
  activeMinecraftCape,
  activeMinecraftSkin,
  apiResponseError,
  boundedMessage,
  capeFileUrl,
  defaultSkinFile,
  DEFAULT_SKIN_SOURCE,
  defaultSkinTextureKey,
  defaultSkinTextureKeys,
  detectSkinVariantFromSavedSkin,
  downloadBlob,
  fetchSavedSkinPng,
  isPngFile,
  isPngPath,
  lookupCapeFileUrl,
  lookupMinecraftSkin,
  lookupSkinFileUrl,
  nativeDragPositionHitsElement,
  nativeDragTargetElement,
  normalizeSkinUpload,
  replaceSavedSkinTexture,
  resolveUploadSkinVariant,
  savedSkinApplyErrorMessage,
  savedSkinDownloadFilename,
  savedSkinFileUrl,
  savedSkinRecord,
  savedSkinSourceLabel,
  skinActionErrorMessage,
  skinFlushResult,
  skinVariantValue,
  sortSavedSkins,
  stagedSkinPreviewSrc,
  stagedSkinVariant,
  uploadSkinName,
} from './api';
import { CapePicker } from './CapePicker';
import { DEFAULT_SKINS, type DefaultSkin } from '../../default-skins';
import { useSavedSkins } from './hooks';
import { skinSnapshot } from './skin-snapshot';
import { SkinThreePreview } from './SkinThreePreview';
import {
  NO_CAPE_VALUE,
  type MinecraftProfile,
  type MinecraftSkinLookup,
  type SavedSkinRecord,
  type SkinVariant,
  type StagedSkinUpload,
  type UploadSkinVariant,
} from './types';

type StagePreviewExtra =
  | { kind: 'default'; id: string }
  | { kind: 'profile' }
  | { kind: 'lookup' };

function looksLikeUnresolvedDefaultSkin(skin: SavedSkinRecord, defaultKeyLookupComplete: boolean): boolean {
  if (defaultKeyLookupComplete || skin.source !== 'local_upload') return false;
  return DEFAULT_SKINS.some((defaultSkin) => (
    defaultSkin.name === skin.name && defaultSkin.variant === skin.variant
  ));
}

export function menuItemsForSavedSkin({
  skin,
  selectedPreviewEditing,
  onlineReady,
  applying,
  pendingActionBusy,
  queued,
  deleting,
  onView,
  onApply,
  onApplyNow,
  onCancelQueue,
  onEdit,
  onDownload,
  onDelete,
}: {
  skin: SavedSkinRecord;
  selectedPreviewEditing: boolean;
  onlineReady: boolean;
  applying: boolean;
  pendingActionBusy: boolean;
  queued: boolean;
  deleting: boolean;
  onView: () => void;
  onApply: () => void;
  onApplyNow: () => void;
  onCancelQueue: () => void;
  onEdit: () => void;
  onDownload: () => void;
  onDelete: () => void;
}): ContextMenuItem[] {
  const applied = Boolean(skin.applied_at);
  const items: ContextMenuItem[] = [];

  if (!deleting) {
    items.push({ icon: 'image', label: 'View', onSelect: onView });
  }
  if (queued) {
    if (onlineReady && !pendingActionBusy) {
      items.push({ icon: 'check', label: 'Apply now', onSelect: onApplyNow });
    }
    if (!pendingActionBusy) {
      items.push({ icon: 'x', label: 'Cancel queue', onSelect: onCancelQueue });
    }
  }
  if (onlineReady && !applied && !applying && !queued) {
    items.push({ icon: 'check', label: 'Apply', onSelect: onApply });
  }
  if (!selectedPreviewEditing && !deleting && !applying) {
    items.push({ icon: 'edit', label: 'Edit', onSelect: onEdit });
  }
  if (!deleting) {
    items.push({ icon: 'download', label: 'Download PNG', onSelect: onDownload });
    if (!applied) {
      items.push({ label: '', onSelect: () => {}, divider: true });
      items.push({ icon: 'trash', label: 'Delete', onSelect: onDelete, danger: true });
    }
  }

  return items;
}

export function SavedSkinLibrary({
  onlineReady,
  minecraftProfile,
  playerName,
  onRenameNametag,
  onApplied,
}: {
  onlineReady: boolean;
  minecraftProfile?: MinecraftProfile;
  playerName: string;
  onRenameNametag?: () => void;
  onApplied: () => void;
}): JSX.Element {
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const editTextureInputRef = useRef<HTMLInputElement | null>(null);
  const savedSkinsDropSurfaceRef = useRef<HTMLElement | null>(null);
  const uploadApplyAfterSaveRef = useRef(false);
  const uploadDragDepthRef = useRef(0);
  const nativeDraggedSkinPathsRef = useRef<string[]>([]);
  const nativeUploadBusyRef = useRef(false);
  const nativeEditBusyRef = useRef(false);
  const editKeyRef = useRef<string | null>(null);
  const stageUploadFileRef = useRef<((file: File, applyAfterSave: boolean) => void) | null>(null);
  const stageEditReplacementFileRef = useRef<((file: File) => void) | null>(null);
  const editReplacementDragDepthRef = useRef(0);
  const stagedUploadUrlRef = useRef<string | null>(null);
  const stagedUploadTokenRef = useRef(0);
  const editReplacementUrlRef = useRef<string | null>(null);
  const editReplacementTokenRef = useRef(0);
  const editDetectTokenRef = useRef(0);
  const {
    skins,
    pendingApplyKey,
    state,
    error,
    refresh,
    setPendingApplyKey: setLocalPendingApplyKey,
  } = useSavedSkins();
  const [skinName, setSkinName] = useState('');
  const [lookupUsername, setLookupUsername] = useState('');
  const [lookupProfile, setLookupProfile] = useState<MinecraftSkinLookup | null>(null);
  const [lookupState, setLookupState] = useState<'idle' | 'loading' | 'ready' | 'error'>('idle');
  const [lookupError, setLookupError] = useState<string | null>(null);
  const [lookupVariant, setLookupVariant] = useState<SkinVariant>('classic');
  const [uploadVariant, setUploadVariant] = useState<UploadSkinVariant>('auto');
  const [stagedCapeId, setStagedCapeId] = useState<string>(NO_CAPE_VALUE);
  const [stagedUpload, setStagedUpload] = useState<StagedSkinUpload | null>(null);
  const [editReplacement, setEditReplacement] = useState<StagedSkinUpload | null>(null);
  const [busy, setBusy] = useState(false);
  const [profileBusy, setProfileBusy] = useState(false);
  const [profileResetBusy, setProfileResetBusy] = useState(false);
  const [profileCapeResetBusy, setProfileCapeResetBusy] = useState(false);
  const [lookupBusy, setLookupBusy] = useState(false);
  const [uploadDragActive, setUploadDragActive] = useState(false);
  const [editReplacementDragActive, setEditReplacementDragActive] = useState(false);
  const [message, setMessage] = useState<{
    tone: 'ok' | 'err';
    text: string;
  } | null>(null);
  const [editKey, setEditKey] = useState<string | null>(null);
  const [editName, setEditName] = useState('');
  const [editVariant, setEditVariant] = useState<SkinVariant>('classic');
  const [editCapeId, setEditCapeId] = useState<string>(NO_CAPE_VALUE);
  const [editBusyKey, setEditBusyKey] = useState<string | null>(null);
  const [editDetectBusyKey, setEditDetectBusyKey] = useState<string | null>(null);
  const [editDetectError, setEditDetectError] = useState<string | null>(null);
  const [deleteKey, setDeleteKey] = useState<string | null>(null);
  const [applyKey, setApplyKey] = useState<string | null>(null);
  const [downloadKey, setDownloadKey] = useState<string | null>(null);
  const [flushBusy, setFlushBusy] = useState(false);
  const [cancelPendingBusy, setCancelPendingBusy] = useState(false);
  const [capeBusy, setCapeBusy] = useState(false);
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const [previewExtra, setPreviewExtra] = useState<StagePreviewExtra | null>(null);
  const [defaultKeyById, setDefaultKeyById] = useState<Map<string, string>>(() => new Map());
  const [defaultKeyLookupComplete, setDefaultKeyLookupComplete] = useState(false);
  const profileSkin = activeMinecraftSkin(minecraftProfile);
  const profileCape = activeMinecraftCape(minecraftProfile);
  const availableCapes = minecraftProfile?.capes ?? [];
  const capeById = useMemo(
    () => new Map(availableCapes.map((cape) => [cape.id, cape])),
    [availableCapes],
  );
  const profileSkinVariant = skinVariantValue(profileSkin?.variant);
  const trimmedName = skinName.trim();
  const trimmedLookupUsername = lookupUsername.trim();
  const trimmedEditName = editName.trim();
  const lookupUsernameError = trimmedLookupUsername ? validateUsername(trimmedLookupUsername) : null;
  const profileSkinFileSrc = profileSkin ? apiResourceUrl('/skin/profile/file') : undefined;
  const canUpload = !busy && !profileBusy && !profileResetBusy && !profileCapeResetBusy && !lookupBusy;
  const canSaveProfileSkin = onlineReady && Boolean(profileSkin) && !busy && !profileBusy && !profileResetBusy && !profileCapeResetBusy && !lookupBusy;
  const canResetProfileSkin = onlineReady && Boolean(profileSkin) && !busy && !profileBusy && !profileResetBusy && !profileCapeResetBusy && !lookupBusy;
  const canResetProfileCape = onlineReady && Boolean(profileCape) && !busy && !profileBusy && !profileResetBusy && !profileCapeResetBusy && !lookupBusy;
  const canLookupSkin = Boolean(trimmedLookupUsername)
    && !lookupUsernameError
    && !busy
    && !profileBusy
    && !profileResetBusy
    && !profileCapeResetBusy
    && !lookupBusy;
  const canSaveLookupSkin = Boolean(lookupProfile)
    && lookupState === 'ready'
    && !busy
    && !profileBusy
    && !profileResetBusy
    && !profileCapeResetBusy
    && !lookupBusy;
  const equippedSkin = skins.find((skin) => Boolean(skin.applied_at)) ?? null;
  const pendingApplySkin = skins.find((skin) => skin.texture_key === pendingApplyKey) ?? null;
  const selectedSavedSkin = selectedKey
    ? skins.find((skin) => skin.texture_key === selectedKey) ?? null
    : null;
  const selectedSkin = previewExtra?.kind === 'default'
    ? null
    : selectedSavedSkin
      ?? pendingApplySkin
      ?? equippedSkin
      ?? skins[0]
      ?? null;
  const sortedSkins = useMemo(() => sortSavedSkins(skins, 'recent'), [skins]);
  const savedSkinByKey = useMemo(
    () => new Map(skins.map((skin) => [skin.texture_key, skin])),
    [skins],
  );
  const defaultIdByKey = useMemo(() => {
    const ids = new Map<string, string>();
    for (const [id, key] of defaultKeyById) ids.set(key, id);
    return ids;
  }, [defaultKeyById]);
  const librarySkins = useMemo(
    () => sortedSkins.filter((skin) => (
      skin.source !== DEFAULT_SKIN_SOURCE
        && !defaultIdByKey.has(skin.texture_key)
        && !looksLikeUnresolvedDefaultSkin(skin, defaultKeyLookupComplete)
    )),
    [sortedSkins, defaultIdByKey, defaultKeyLookupComplete],
  );
  const savedRecordForDefault = (id: string): SavedSkinRecord | null => {
    const key = defaultKeyById.get(id);
    if (key) return savedSkinByKey.get(key) ?? null;
    const defaultSkin = DEFAULT_SKINS.find((skin) => skin.id === id);
    if (!defaultSkin) return null;
    return skins.find((skin) => (
      skin.source === DEFAULT_SKIN_SOURCE
        && skin.name === defaultSkin.name
        && skin.variant === defaultSkin.variant
    )) ?? null;
  };
  const selectedDefault = previewExtra?.kind === 'default'
    ? DEFAULT_SKINS.find((skin) => skin.id === previewExtra.id) ?? null
    : null;
  const selectedPreviewEditing = Boolean(selectedSkin && editKey === selectedSkin.texture_key);
  const selectedQueued = Boolean(selectedSkin && selectedSkin.texture_key === pendingApplyKey);
  const stagedVariant = stagedUpload ? stagedSkinVariant(stagedUpload, uploadVariant) : null;
  const stagedName = stagedUpload
    ? trimmedName || uploadSkinName(stagedUpload.file) || 'Uploaded skin'
    : '';
  const stagedVariantReady = Boolean(
    stagedUpload && (uploadVariant !== 'auto' || !stagedUpload.detectingVariant),
  );
  const stagedValidated = stagedUpload?.normalizeStatus === 'ready';
  const stagedCanSave = Boolean(stagedUpload && canUpload && stagedVariantReady && stagedValidated);
  const editReplacementReady = !editReplacement || editReplacement.normalizeStatus === 'ready';
  const showProfileSelectedPreview = Boolean(state === 'ready' && skins.length === 0 && profileSkin && minecraftProfile);
  const capeSrcForId = (capeId: string | null | undefined): string | undefined => {
    if (!capeId) return undefined;
    const cape = capeById.get(capeId);
    return cape ? capeFileUrl(cape) : undefined;
  };
  const stagedCapeSelected = stagedCapeId !== NO_CAPE_VALUE;
  const stagedCapeSrc = capeSrcForId(stagedCapeSelected ? stagedCapeId : null);

  const clearStagedUpload = (): void => {
    stagedUploadTokenRef.current += 1;
    if (stagedUploadUrlRef.current) {
      URL.revokeObjectURL(stagedUploadUrlRef.current);
      stagedUploadUrlRef.current = null;
    }
    setStagedUpload(null);
    setStagedCapeId(NO_CAPE_VALUE);
    uploadApplyAfterSaveRef.current = false;
    if (fileInputRef.current) fileInputRef.current.value = '';
  };

  const clearEditReplacement = (): void => {
    editReplacementTokenRef.current += 1;
    if (editReplacementUrlRef.current) {
      URL.revokeObjectURL(editReplacementUrlRef.current);
      editReplacementUrlRef.current = null;
    }
    setEditReplacement(null);
    setEditReplacementDragActive(false);
    editReplacementDragDepthRef.current = 0;
    if (editTextureInputRef.current) editTextureInputRef.current.value = '';
  };

  useEffect(() => {
    if (!local.selectedSkin.startsWith('default:')) return;
    const id = local.selectedSkin.slice('default:'.length);
    if (DEFAULT_SKINS.some((skin) => skin.id === id)) {
      setSelectedKey(null);
      setPreviewExtra({ kind: 'default', id });
    }
  }, []);

  useEffect(() => {
    let active = true;
    const timer = window.setTimeout(() => {
      void defaultSkinTextureKeys()
      .then((keys) => {
        if (active) {
          setDefaultKeyById(keys);
          setDefaultKeyLookupComplete(true);
        }
      })
      .catch(() => {});
    }, 700);
    return () => {
      active = false;
      window.clearTimeout(timer);
    };
  }, []);

  useEffect(() => {
    if (state !== 'ready') return;
    if (skins.length === 0) {
      if (selectedKey !== null) setSelectedKey(null);
      if (!local.selectedSkin.startsWith('default:')) resetSelectedSkin();
      return;
    }
    if (selectedKey && skins.some((skin) => skin.texture_key === selectedKey)) return;
    if (local.selectedSkin.startsWith('default:')) return;
    const selectedSavedKey = local.selectedSkin.startsWith('saved:')
      ? local.selectedSkin.slice('saved:'.length)
      : null;
    const next = (selectedSavedKey ? skins.find((skin) => skin.texture_key === selectedSavedKey) : undefined)
      ?? skins.find((skin) => Boolean(skin.applied_at))
      ?? skins[0];
    setSelectedKey(next.texture_key);
    setSelectedSkin(`saved:${next.texture_key}`);
  }, [skins, selectedKey, state]);

  useEffect(() => {
    nativeUploadBusyRef.current = busy || profileBusy || profileResetBusy || profileCapeResetBusy || lookupBusy;
  }, [busy, lookupBusy, profileBusy, profileCapeResetBusy, profileResetBusy]);

  useEffect(() => {
    nativeEditBusyRef.current = Boolean(editBusyKey || editDetectBusyKey);
  }, [editBusyKey, editDetectBusyKey]);

  useEffect(() => {
    editKeyRef.current = editKey;
  }, [editKey]);

  useEffect(() => {
    if (!pendingApplyKey) return;
    const timer = window.setTimeout(() => {
      refresh();
      onApplied();
    }, 11_500);
    return () => window.clearTimeout(timer);
  }, [onApplied, pendingApplyKey, refresh]);

  useEffect(() => {
    return () => {
      stagedUploadTokenRef.current += 1;
      if (stagedUploadUrlRef.current) {
        URL.revokeObjectURL(stagedUploadUrlRef.current);
        stagedUploadUrlRef.current = null;
      }
      editReplacementTokenRef.current += 1;
      if (editReplacementUrlRef.current) {
        URL.revokeObjectURL(editReplacementUrlRef.current);
        editReplacementUrlRef.current = null;
      }
    };
  }, []);

  const upload = async (
    file: File,
    applyAfterSave = false,
    variantOverride?: SkinVariant,
    capeIdOverride = NO_CAPE_VALUE,
    sourceOverride?: string,
  ): Promise<void> => {
    const name = trimmedName || uploadSkinName(file);
    if (!name) {
      setMessage({ tone: 'err', text: 'Name the skin before uploading.' });
      return;
    }

    setBusy(true);
    setMessage(null);
    try {
      const resolvedVariant = variantOverride ?? await resolveUploadSkinVariant(file, uploadVariant);
      const params = new URLSearchParams({ name, variant: resolvedVariant });
      if (capeIdOverride !== NO_CAPE_VALUE) params.set('cape_id', capeIdOverride);
      if (sourceOverride) params.set('source', sourceOverride);
      const response = await fetch(apiUrl(`/skins?${params.toString()}`), {
        method: 'POST',
        headers: { 'Content-Type': 'image/png' },
        body: file,
      });
      const payload = await response.json().catch(() => undefined);
      if (!response.ok) {
        throw apiResponseError(response, payload, `Upload failed with HTTP ${response.status}`);
      }
      const saved = savedSkinRecord(payload);
      setSkinName('');
      clearStagedUpload();
      if (saved) {
        setSelectedKey(saved.texture_key);
        setPreviewExtra(null);
        setSelectedSkin(`saved:${saved.texture_key}`);
      }
      if (saved && applyAfterSave) {
        try {
          await applySavedSkin(saved.texture_key);
          toast('Skin will apply shortly');
        } catch (err) {
          setSelectedKey(saved.texture_key);
          refresh();
          setMessage({ tone: 'err', text: savedSkinApplyErrorMessage(err) });
        }
      } else {
        refresh();
        toast('Skin added to your library');
      }
    } catch (err) {
      setMessage({
        tone: 'err',
        text: skinActionErrorMessage(err, 'Could not save skin.'),
      });
    } finally {
      setBusy(false);
      if (fileInputRef.current) fileInputRef.current.value = '';
    }
  };

  const deleteSkin = async (textureKey: string, skinName?: string): Promise<void> => {
    setDeleteKey(textureKey);
    setMessage(null);
    try {
      await api('DELETE', `/skins/${textureKey}`);
      if (selectedKey === textureKey) setSelectedKey(null);
      if (local.selectedSkin === `saved:${textureKey}`) resetSelectedSkin();
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
    if (!onlineReady) return;

    setProfileBusy(true);
    setMessage(null);
    try {
      const request: { variant?: SkinVariant } = {};
      if (profileSkin) request.variant = profileSkinVariant;
      const payload = await api('POST', '/skins/from-profile', request);
      const saved = savedSkinRecord(payload);
      if (saved) {
        setSelectedKey(saved.texture_key);
        setPreviewExtra(null);
        setSelectedSkin(`saved:${saved.texture_key}`);
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
    if (!onlineReady || !profileSkin) return;
    const ok = await showConfirm(
      'Reset the active Minecraft profile skin to the default skin? Croopor will save the current profile skin locally first.',
      { title: 'Reset profile skin', destructive: true, confirmText: 'Reset' },
    );
    if (!ok) return;

    setProfileResetBusy(true);
    setMessage(null);
    try {
      await api('POST', '/skin/profile/reset', {});
      refresh();
      onApplied();
      toast('Profile skin reset to default');
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
    if (!onlineReady || !profileCape) return;
    const ok = await showConfirm(
      'Remove the active Minecraft profile cape? Croopor will save the current skin and cape pairing locally first.',
      { title: 'Reset profile cape', destructive: true, confirmText: 'Reset cape' },
    );
    if (!ok) return;

    setProfileCapeResetBusy(true);
    setMessage(null);
    try {
      await api('POST', '/skin/cape/reset', {});
      refresh();
      onApplied();
      toast('Profile cape reset');
    } catch (err) {
      setMessage({
        tone: 'err',
        text: skinActionErrorMessage(err, 'Could not reset Minecraft profile cape.'),
      });
    } finally {
      setProfileCapeResetBusy(false);
    }
  };

  const lookupSkin = async (): Promise<void> => {
    if (!trimmedLookupUsername) {
      setLookupState('error');
      setLookupError('Enter a Minecraft username.');
      return;
    }
    if (lookupUsernameError) {
      setLookupState('error');
      setLookupError(lookupUsernameError);
      return;
    }

    setLookupBusy(true);
    setLookupState('loading');
    setLookupError(null);
    setLookupProfile(null);
    setMessage(null);
    try {
      const profile = await lookupMinecraftSkin(trimmedLookupUsername);
      setLookupProfile(profile);
      setLookupState('ready');
      setLookupVariant(profile.variant);
      setPreviewExtra({ kind: 'lookup' });
    } catch (err) {
      setLookupState('error');
      setLookupError(skinActionErrorMessage(err, 'Could not find that player skin.'));
    } finally {
      setLookupBusy(false);
    }
  };

  const dismissLookup = (): void => {
    setLookupProfile(null);
    setLookupState('idle');
    setLookupError(null);
    setLookupUsername('');
    setPreviewExtra((current) => (current?.kind === 'lookup' ? null : current));
  };

  const saveUsernameSkin = async (applyAfterSave: boolean): Promise<void> => {
    if (!lookupProfile) {
      setMessage({ tone: 'err', text: 'Search for a Minecraft profile before saving this skin.' });
      return;
    }

    setLookupBusy(true);
    setMessage(null);
    try {
      const request: { username: string; variant?: SkinVariant } = {
        username: lookupProfile.username,
        variant: lookupVariant,
      };
      const payload = await api('POST', '/skins/from-username', request);
      const saved = savedSkinRecord(payload);
      if (saved) {
        setSelectedKey(saved.texture_key);
        setSelectedSkin(`saved:${saved.texture_key}`);
      }
      setLookupUsername('');
      setLookupVariant('classic');
      setLookupProfile(null);
      setLookupState('idle');
      setLookupError(null);
      setPreviewExtra(null);
      if (saved && applyAfterSave) {
        try {
          await applySavedSkin(saved.texture_key);
          toast(`${request.username}'s skin will apply shortly`);
        } catch (err) {
          setSelectedKey(saved.texture_key);
          refresh();
          setMessage({ tone: 'err', text: savedSkinApplyErrorMessage(err) });
        }
      } else {
        refresh();
        toast(`${request.username}'s skin added to your library`);
      }
    } catch (err) {
      setMessage({
        tone: 'err',
        text: skinActionErrorMessage(err, 'Could not save player skin.'),
      });
    } finally {
      setLookupBusy(false);
    }
  };

  const hasUnsavedSkinEdit = (): boolean => {
    if (!editKey) return false;
    const skin = skins.find((candidate) => candidate.texture_key === editKey);
    if (!skin) return Boolean(editReplacement);
    const nextCapeId = editCapeId === NO_CAPE_VALUE ? null : editCapeId;
    return Boolean(editReplacement) ||
      trimmedEditName !== skin.name ||
      editVariant !== skin.variant ||
      nextCapeId !== (skin.cape_id ?? null);
  };

  const resetSkinEditState = (): void => {
    editDetectTokenRef.current += 1;
    clearEditReplacement();
    setEditKey(null);
    setEditName('');
    setEditVariant('classic');
    setEditCapeId(NO_CAPE_VALUE);
    setEditDetectBusyKey(null);
    setEditDetectError(null);
  };

  const closeSkinEditBeforeChanging = async (): Promise<boolean> => {
    if (!editKey) return true;
    if (hasUnsavedSkinEdit()) {
      const ok = await showConfirm(
        'Discard unsaved skin edits? Your local skin record will stay unchanged.',
        { title: 'Discard skin edits', destructive: true, confirmText: 'Discard' },
      );
      if (!ok) return false;
    }
    resetSkinEditState();
    return true;
  };

  const startEdit = async (skin: SavedSkinRecord): Promise<void> => {
    if (editKey === skin.texture_key) return;
    const ok = await closeSkinEditBeforeChanging();
    if (!ok) return;

    editDetectTokenRef.current += 1;
    clearEditReplacement();
    setEditKey(skin.texture_key);
    setEditName(skin.name);
    setEditVariant(skin.variant === 'slim' ? 'slim' : 'classic');
    setEditCapeId(skin.cape_id ?? NO_CAPE_VALUE);
    setEditDetectBusyKey(null);
    setEditDetectError(null);
    setMessage(null);
  };

  const cancelEdit = (): void => {
    resetSkinEditState();
  };

  const detectSavedSkinModel = async (skin: SavedSkinRecord): Promise<void> => {
    const token = editDetectTokenRef.current + 1;
    editDetectTokenRef.current = token;
    setEditDetectBusyKey(skin.texture_key);
    setEditDetectError(null);
    setMessage(null);
    try {
      const detectedVariant = await detectSkinVariantFromSavedSkin(skin);
      if (token !== editDetectTokenRef.current) return;
      setEditVariant(detectedVariant);
    } catch (err) {
      if (token !== editDetectTokenRef.current) return;
      setEditDetectError(
        boundedMessage(err instanceof Error ? err.message : undefined, 'Could not detect skin model.'),
      );
    } finally {
      if (token === editDetectTokenRef.current) setEditDetectBusyKey(null);
    }
  };

  const stageEditReplacementFile = (file: File): void => {
    if (!editKey) return;
    if (!isPngFile(file)) {
      setMessage({ tone: 'err', text: 'Choose a PNG skin file.' });
      if (editTextureInputRef.current) editTextureInputRef.current.value = '';
      return;
    }
    if (editBusyKey) {
      setMessage({ tone: 'err', text: 'Wait for the current skin edit to finish.' });
      if (editTextureInputRef.current) editTextureInputRef.current.value = '';
      return;
    }

    const objectUrl = URL.createObjectURL(file);
    editReplacementTokenRef.current += 1;
    const token = editReplacementTokenRef.current;
    if (editReplacementUrlRef.current) URL.revokeObjectURL(editReplacementUrlRef.current);
    editReplacementUrlRef.current = objectUrl;
    setMessage(null);
    setEditDetectError(null);
    setEditReplacement({
      file,
      objectUrl,
      detectedVariant: 'classic',
      detectingVariant: true,
      normalizeStatus: 'checking',
      applyAfterSave: false,
    });

    void normalizeSkinUpload(file).then((metadata) => {
      if (token !== editReplacementTokenRef.current) return;
      setEditVariant(metadata.variantSuggestion);
      setEditReplacement((current) => current?.objectUrl === objectUrl
        ? {
            ...current,
            detectedVariant: metadata.variantSuggestion,
            detectingVariant: false,
            normalizeStatus: 'ready',
            normalizeError: undefined,
            textureKey: metadata.textureKey,
            originalWidth: metadata.originalWidth,
            originalHeight: metadata.originalHeight,
            normalizedByteSize: metadata.normalizedByteSize,
            normalizedDataUrl: metadata.normalizedDataUrl,
          }
        : current);
    }).catch((err) => {
      if (token !== editReplacementTokenRef.current) return;
      setEditReplacement((current) => current?.objectUrl === objectUrl
        ? {
            ...current,
            detectingVariant: false,
            normalizeStatus: 'error',
            normalizeError: boundedMessage(err instanceof Error ? err.message : undefined, 'Skin validation failed.'),
          }
        : current);
    });
  };

  const openEditTexturePicker = (): void => {
    if (editTextureInputRef.current) editTextureInputRef.current.value = '';
    void (async () => {
      try {
        const nativeFile = await pickNativeSkinFile();
        if (nativeFile) {
          stageEditReplacementFile(nativeFile);
          return;
        }
        if (nativeFile === null) return;
        editTextureInputRef.current?.click();
      } catch (err) {
        setMessage({
          tone: 'err',
          text: boundedMessage(err instanceof Error ? err.message : undefined, 'Could not open skin file.'),
        });
      }
    })();
  };

  const handleEditReplacementDrop = (event: DragEvent): void => {
    event.preventDefault();
    event.stopPropagation();
    editReplacementDragDepthRef.current = 0;
    setEditReplacementDragActive(false);

    const files = event.dataTransfer?.files;
    if (!files || files.length === 0) return;
    if (files.length !== 1) {
      setMessage({ tone: 'err', text: 'Drop one PNG skin file to replace this texture.' });
      return;
    }

    stageEditReplacementFile(files[0]);
  };

  const handleEditReplacementDragEnter = (event: DragEvent): void => {
    if (!Array.from(event.dataTransfer?.types ?? []).includes('Files')) return;
    event.preventDefault();
    event.stopPropagation();
    editReplacementDragDepthRef.current += 1;
    setEditReplacementDragActive(true);
  };

  const handleEditReplacementDragOver = (event: DragEvent): void => {
    if (!Array.from(event.dataTransfer?.types ?? []).includes('Files')) return;
    event.preventDefault();
    event.stopPropagation();
    if (event.dataTransfer) event.dataTransfer.dropEffect = editBusyKey ? 'none' : 'copy';
  };

  const handleEditReplacementDragLeave = (event: DragEvent): void => {
    if (!Array.from(event.dataTransfer?.types ?? []).includes('Files')) return;
    event.preventDefault();
    event.stopPropagation();
    editReplacementDragDepthRef.current = Math.max(0, editReplacementDragDepthRef.current - 1);
    if (editReplacementDragDepthRef.current === 0) setEditReplacementDragActive(false);
  };

  const saveSkinMetadata = async (textureKey: string, applyAfterSave = false): Promise<void> => {
    const skin = skins.find((candidate) => candidate.texture_key === textureKey);
    if (skin && !savedSkinEditHasChanges(skin)) {
      setMessage({ tone: 'err', text: 'Make an edit to the skin before saving.' });
      return;
    }
    if (!trimmedEditName) {
      setMessage({ tone: 'err', text: 'Name the skin before saving.' });
      return;
    }
    if (editReplacement && editReplacement.normalizeStatus !== 'ready') {
      setMessage({ tone: 'err', text: 'Wait for the replacement PNG to validate before saving.' });
      return;
    }

    setEditBusyKey(textureKey);
    setMessage(null);
    try {
      const previousCapeId = skin?.cape_id ?? null;
      const nextCapeId = editCapeId === NO_CAPE_VALUE ? null : editCapeId;
      const profileRelevantEdit = Boolean(
        editReplacement ||
        (skin && editVariant !== skin.variant) ||
        nextCapeId !== previousCapeId
      );
      const shouldReapplyEditedSkin = Boolean(
        skin?.applied_at &&
        profileRelevantEdit,
      );
      const shouldApplyEditedSkin = Boolean(
        onlineReady &&
        (
          (applyAfterSave && !skin?.applied_at) ||
          shouldReapplyEditedSkin
        ),
      );
      let savedTextureKey = textureKey;
      const savedMessage = editReplacement ? 'Skin texture replaced.' : 'Skin details updated.';
      if (editReplacement) {
        const saved = await replaceSavedSkinTexture(textureKey, editReplacement.file, {
          name: trimmedEditName,
          variant: editVariant,
          capeId: nextCapeId === previousCapeId ? undefined : nextCapeId,
        });
        savedTextureKey = saved.texture_key;
        setSelectedKey(saved.texture_key);
        setSelectedSkin(`saved:${saved.texture_key}`);
      } else {
        const payload: { name: string; variant: SkinVariant; cape_id?: string | null } = {
          name: trimmedEditName,
          variant: editVariant,
        };
        if (skin && editCapeId !== (skin.cape_id ?? NO_CAPE_VALUE)) {
          payload.cape_id = editCapeId === NO_CAPE_VALUE ? null : editCapeId;
        }
        const updated = savedSkinRecord(await api('PUT', `/skins/${textureKey}`, payload));
        if (!updated) throw new Error('Skin details update returned an invalid response.');
        savedTextureKey = updated.texture_key;
      }
      cancelEdit();
      if (shouldApplyEditedSkin) {
        try {
          await applySavedSkin(savedTextureKey);
          toast(`${savedMessage} It will apply shortly`);
        } catch (err) {
          refresh();
          setMessage({
            tone: 'err',
            text: `${savedMessage} Could not queue the skin: ${skinActionErrorMessage(err, 'apply failed.')}`,
          });
        }
      } else {
        refresh();
        toast(savedMessage);
      }
    } catch (err) {
      setMessage({
        tone: 'err',
        text: skinActionErrorMessage(err, 'Could not update skin details.'),
      });
    } finally {
      setEditBusyKey(null);
    }
  };

  const applySavedSkin = async (textureKey: string, options: { select?: boolean } = {}): Promise<void> => {
    await api('POST', `/skins/${textureKey}/apply?defer=true`);
    setLocalPendingApplyKey(textureKey);
    if (options.select !== false) {
      setSelectedKey(textureKey);
      setSelectedSkin(`saved:${textureKey}`);
    }
    refresh();
  };

  const viewSavedSkin = async (textureKey: string): Promise<void> => {
    if (editKey && editKey !== textureKey) {
      const ok = await closeSkinEditBeforeChanging();
      if (!ok) return;
    }

    setSelectedKey(textureKey);
    setPreviewExtra(null);
    setMessage(null);
    setSelectedSkin(`saved:${textureKey}`);
  };

  const viewDefaultSkin = (id: string): void => {
    setSelectedKey(null);
    setPreviewExtra({ kind: 'default', id });
    setMessage(null);
    setSelectedSkin(`default:${id}`);
  };

  const viewProfileSkin = (): void => {
    setPreviewExtra({ kind: 'profile' });
    setMessage(null);
  };

  const applyDefaultSkin = async (skin: DefaultSkin): Promise<void> => {
    setSelectedKey(null);
    setPreviewExtra({ kind: 'default', id: skin.id });
    setSelectedSkin(`default:${skin.id}`);
    const existingKey = await defaultSkinTextureKey(skin).catch(() => defaultKeyById.get(skin.id));
    if (existingKey && defaultKeyById.get(skin.id) !== existingKey) {
      setDefaultKeyById((current) => {
        if (current.get(skin.id) === existingKey) return current;
        const next = new Map(current);
        next.set(skin.id, existingKey);
        return next;
      });
    }
    const existing = existingKey ? savedSkinByKey.get(existingKey) ?? null : null;
    if (existing) {
      setBusy(true);
      setMessage(null);
      try {
        await applySavedSkin(existing.texture_key, { select: false });
        toast('Skin will apply shortly');
      } catch (err) {
        setMessage({
          tone: 'err',
          text: skinActionErrorMessage(err, 'Could not apply skin.'),
        });
      } finally {
        setBusy(false);
      }
      return;
    }
    await upload(await defaultSkinFile(skin), true, skin.variant, NO_CAPE_VALUE, DEFAULT_SKIN_SOURCE);
    setSelectedKey(null);
    setPreviewExtra({ kind: 'default', id: skin.id });
    setSelectedSkin(`default:${skin.id}`);
  };

  const applySkin = async (textureKey: string): Promise<void> => {
    const skin = skins.find((saved) => saved.texture_key === textureKey);
    if (skin?.applied_at) return;
    const ok = await closeSkinEditBeforeChanging();
    if (!ok) return;

    setApplyKey(textureKey);
    setMessage(null);
    try {
      await applySavedSkin(textureKey);
      toast('Skin will apply shortly');
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
      const updated = savedSkinRecord(await api('PUT', `/skins/${selectedSkin.texture_key}`, {
        name: selectedSkin.name,
        variant: selectedSkin.variant,
        cape_id: nextCapeId,
      }));
      if (!updated) throw new Error('Cape update returned an invalid response.');
      if (selectedSkin.applied_at && onlineReady) {
        await applySavedSkin(updated.texture_key);
        toast('Cape will apply shortly');
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
        toast('Skin applied');
      } else {
        setLocalPendingApplyKey(null);
        toast('No skin change was pending');
      }
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
      await api('DELETE', '/skins/pending');
      setLocalPendingApplyKey(null);
      refresh();
      toast('Skin change canceled');
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
    resetSelectedSkin();
    setSelectedKey(null);
    setPreviewExtra({ kind: 'default', id: 'steve' });
    setMessage(null);
  };

  const stageUploadFile = (file: File, applyAfterSave: boolean): void => {
    if (!isPngFile(file)) {
      setMessage({ tone: 'err', text: 'Upload a PNG skin file.' });
      if (fileInputRef.current) fileInputRef.current.value = '';
      return;
    }
    if (busy || profileBusy || lookupBusy) {
      setMessage({ tone: 'err', text: 'Wait for the current skin action to finish.' });
      if (fileInputRef.current) fileInputRef.current.value = '';
      return;
    }

    const objectUrl = URL.createObjectURL(file);
    stagedUploadTokenRef.current += 1;
    const token = stagedUploadTokenRef.current;
    if (stagedUploadUrlRef.current) URL.revokeObjectURL(stagedUploadUrlRef.current);
    stagedUploadUrlRef.current = objectUrl;
    setMessage(null);
    setStagedCapeId(NO_CAPE_VALUE);
    setStagedUpload({
      file,
      objectUrl,
      detectedVariant: 'classic',
      detectingVariant: true,
      normalizeStatus: 'checking',
      applyAfterSave,
    });

    void normalizeSkinUpload(file).then((metadata) => {
      if (token !== stagedUploadTokenRef.current) return;
      setStagedUpload((current) => current?.objectUrl === objectUrl
        ? {
            ...current,
            detectedVariant: metadata.variantSuggestion,
            detectingVariant: false,
            normalizeStatus: 'ready',
            normalizeError: undefined,
            textureKey: metadata.textureKey,
            originalWidth: metadata.originalWidth,
            originalHeight: metadata.originalHeight,
            normalizedByteSize: metadata.normalizedByteSize,
            normalizedDataUrl: metadata.normalizedDataUrl,
          }
        : current);
    }).catch((err) => {
      if (token !== stagedUploadTokenRef.current) return;
      setStagedUpload((current) => current?.objectUrl === objectUrl
        ? {
            ...current,
            detectingVariant: false,
            normalizeStatus: 'error',
            normalizeError: boundedMessage(err instanceof Error ? err.message : undefined, 'Skin validation failed.'),
          }
        : current);
    });
  };

  useEffect(() => {
    stageUploadFileRef.current = stageUploadFile;
    stageEditReplacementFileRef.current = stageEditReplacementFile;
  });

  useEffect(() => {
    let active = true;
    let subscription: { close(): void } | null = null;

    const pathsForPayload = (payload: NativeDragDropPayload): string[] => (
      payload.paths.length > 0 ? payload.paths : nativeDraggedSkinPathsRef.current
    );

    const editDropTextureKeyForPayload = (payload: NativeDragDropPayload): string | null => {
      const target = nativeDragTargetElement<HTMLElement>(
        payload.position,
        '[data-saved-skin-edit-drop-surface]',
      );
      const textureKey = target?.getAttribute('data-saved-skin-edit-drop-surface') ?? null;
      return textureKey && textureKey === editKeyRef.current ? textureKey : null;
    };

    const handleNativeDragDrop = (payload: NativeDragDropPayload): void => {
      if (!active) return;
      if (payload.type === 'leave') {
        nativeDraggedSkinPathsRef.current = [];
        setUploadDragActive(false);
        setEditReplacementDragActive(false);
        return;
      }

      const paths = pathsForPayload(payload);
      const skinPaths = paths.filter(isPngPath);
      const editDropTextureKey = editDropTextureKeyForPayload(payload);
      const overDropSurface = nativeDragPositionHitsElement(
        payload.position,
        savedSkinsDropSurfaceRef.current,
      );
      const overEditDropSurface = Boolean(editDropTextureKey);

      if (payload.type === 'enter') {
        nativeDraggedSkinPathsRef.current = payload.paths;
        setEditReplacementDragActive(skinPaths.length > 0 && overEditDropSurface);
        setUploadDragActive(skinPaths.length > 0 && !overEditDropSurface && overDropSurface);
        return;
      }

      if (payload.type === 'over') {
        setEditReplacementDragActive(skinPaths.length > 0 && overEditDropSurface);
        setUploadDragActive(skinPaths.length > 0 && !overEditDropSurface && overDropSurface);
        return;
      }

      nativeDraggedSkinPathsRef.current = [];
      setUploadDragActive(false);
      setEditReplacementDragActive(false);
      if (payload.type !== 'drop' || (!overDropSurface && !overEditDropSurface)) return;
      if (skinPaths.length === 0) return;
      if (skinPaths.length !== 1) {
        setMessage({
          tone: 'err',
          text: overEditDropSurface ? 'Drop one PNG skin file to replace this texture.' : 'Drop one PNG skin file.',
        });
        return;
      }
      if (overEditDropSurface) {
        if (nativeEditBusyRef.current) {
          setMessage({ tone: 'err', text: 'Wait for the current skin edit to finish.' });
          return;
        }

        void (async () => {
          try {
            const file = await readNativeSkinFile(skinPaths[0]);
            if (!active || !file) return;
            stageEditReplacementFileRef.current?.(file);
          } catch (err) {
            if (!active) return;
            setMessage({
              tone: 'err',
              text: boundedMessage(err instanceof Error ? err.message : undefined, 'Could not read dropped skin file.'),
            });
          }
        })();
        return;
      }
      if (nativeUploadBusyRef.current) {
        setMessage({ tone: 'err', text: 'Wait for the current skin action to finish.' });
        return;
      }

      void (async () => {
        try {
          const file = await readNativeSkinFile(skinPaths[0]);
          if (!active || !file) return;
          stageUploadFileRef.current?.(file, false);
        } catch (err) {
          if (!active) return;
          setMessage({
            tone: 'err',
            text: boundedMessage(err instanceof Error ? err.message : undefined, 'Could not read dropped skin file.'),
          });
        }
      })();
    };

    void onNativeDragDrop(handleNativeDragDrop).then((nextSubscription) => {
      if (!active) {
        nextSubscription?.close();
        return;
      }
      subscription = nextSubscription;
    });

    return () => {
      active = false;
      nativeDraggedSkinPathsRef.current = [];
      setUploadDragActive(false);
      setEditReplacementDragActive(false);
      subscription?.close();
    };
  }, []);

  const saveStagedUpload = (applyAfterSave: boolean): void => {
    if (!stagedUpload || !stagedVariant || !stagedCanSave) return;
    if (applyAfterSave && !onlineReady) return;
    void upload(stagedUpload.file, applyAfterSave, stagedVariant, stagedCapeId);
  };

  const handleUploadDrop = (event: DragEvent): void => {
    event.preventDefault();
    event.stopPropagation();
    uploadDragDepthRef.current = 0;
    setUploadDragActive(false);

    const files = event.dataTransfer?.files;
    if (!files || files.length === 0) return;
    if (files.length !== 1) {
      setMessage({ tone: 'err', text: 'Drop one PNG skin file.' });
      return;
    }

    stageUploadFile(files[0], false);
  };

  const handleUploadDragEnter = (event: DragEvent): void => {
    if (!Array.from(event.dataTransfer?.types ?? []).includes('Files')) return;
    event.preventDefault();
    event.stopPropagation();
    uploadDragDepthRef.current += 1;
    setUploadDragActive(true);
  };

  const handleUploadDragOver = (event: DragEvent): void => {
    if (!Array.from(event.dataTransfer?.types ?? []).includes('Files')) return;
    event.preventDefault();
    if (event.dataTransfer) event.dataTransfer.dropEffect = busy || profileBusy || lookupBusy ? 'none' : 'copy';
  };

  const handleUploadDragLeave = (event: DragEvent): void => {
    if (!Array.from(event.dataTransfer?.types ?? []).includes('Files')) return;
    event.preventDefault();
    event.stopPropagation();
    uploadDragDepthRef.current = Math.max(0, uploadDragDepthRef.current - 1);
    if (uploadDragDepthRef.current === 0) setUploadDragActive(false);
  };

  const handleUploadFile = (file: File, applyAfterSave: boolean): void => {
    stageUploadFile(file, applyAfterSave);
  };

  const openUploadPicker = (applyAfterSave: boolean): void => {
    uploadApplyAfterSaveRef.current = applyAfterSave;
    if (fileInputRef.current) fileInputRef.current.value = '';
    void (async () => {
      try {
        const nativeFile = await pickNativeSkinFile();
        if (nativeFile) {
          handleUploadFile(nativeFile, applyAfterSave);
          uploadApplyAfterSaveRef.current = false;
          return;
        }
        if (nativeFile === null) {
          uploadApplyAfterSaveRef.current = false;
          return;
        }
        fileInputRef.current?.click();
      } catch (err) {
        uploadApplyAfterSaveRef.current = false;
        setMessage({
          tone: 'err',
          text: boundedMessage(err instanceof Error ? err.message : undefined, 'Could not open skin file.'),
        });
      }
    })();
  };

  const savedSkinEditHasChanges = (skin: SavedSkinRecord): boolean => {
    const nextCapeId = editCapeId === NO_CAPE_VALUE ? null : editCapeId;
    return Boolean(editReplacement) ||
      trimmedEditName !== skin.name ||
      editVariant !== skin.variant ||
      nextCapeId !== (skin.cape_id ?? null);
  };
  const editingSkin = editKey ? skins.find((skin) => skin.texture_key === editKey) ?? null : null;
  const lookupPreview = previewExtra?.kind === 'lookup' && lookupState === 'ready' ? lookupProfile : null;
  const profilePreviewActive = Boolean(
    previewExtra?.kind === 'profile' && profileSkin && minecraftProfile,
  );
  const stageDefaultSkin = selectedDefault
    ?? (state === 'ready' && !selectedSkin && !profileSkin
      ? DEFAULT_SKINS[0]
      : null);
  const stageNametag = local.hideSkinNametag ? null : playerName.trim() || null;
  const stageEditingSrc = selectedPreviewEditing && editReplacement
    ? stagedSkinPreviewSrc(editReplacement)
    : null;
  const editPreviewCapeSrc = capeSrcForId(editCapeId === NO_CAPE_VALUE ? null : editCapeId);
  const stageApplyBusy = Boolean(selectedSkin && applyKey === selectedSkin.texture_key);
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

  return (
    <div class="cp-skinpage">
      <input
        ref={editTextureInputRef}
        type="file"
        accept="image/png"
        style={{ display: 'none' }}
        onChange={(event) => {
          const file = event.currentTarget.files?.[0];
          if (file) stageEditReplacementFile(file);
        }}
      />
      <input
        ref={fileInputRef}
        type="file"
        accept="image/png"
        style={{ display: 'none' }}
        onChange={(event) => {
          const file = event.currentTarget.files?.[0];
          if (file) handleUploadFile(file, uploadApplyAfterSaveRef.current);
          uploadApplyAfterSaveRef.current = false;
        }}
      />

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
                onClick={dismissLookup}
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
                    onClick={() => void saveUsernameSkin(false)}
                    title="Keep a copy in your library without wearing it"
                  >
                    Save
                  </Button>
                  <Button
                    variant="primary"
                    icon={lookupBusy ? 'refresh' : 'check'}
                    disabled={!canSaveLookupSkin}
                    onClick={() => void saveUsernameSkin(true)}
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
                  onClick={() => void saveUsernameSkin(false)}
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
                  onClick={resetPreview}
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
                  onClick={() => void applyDefaultSkin(stageDefaultSkin)}
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
                  onClick={() => setPreviewExtra(null)}
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
                onClick={() => void saveProfileSkin()}
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
              capeSrc={selectedPreviewEditing ? editPreviewCapeSrc : capeSrcForId(selectedSkin.cape_id)}
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
                    onClick={() => void cancelPendingApply()}
                    title="Cancel the queued skin change"
                  >
                    Cancel
                  </Button>
                  <Button
                    variant="primary"
                    size="lg"
                    icon={flushBusy ? 'refresh' : 'check'}
                    disabled={!onlineReady || flushBusy || cancelPendingBusy || applyKey !== null}
                    onClick={() => void flushPendingApply()}
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
                    onClick={resetPreview}
                    title="Reset the account avatar to Steve"
                  >
                    Reset
                  </Button>
                  <Button
                    variant="primary"
                    size="lg"
                    icon={stageApplyBusy ? 'refresh' : 'check'}
                    disabled={stageApplyBusy || flushBusy || cancelPendingBusy}
                    onClick={() => void applySkin(selectedSkin.texture_key)}
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
                    onClick={resetPreview}
                    title="Reset the account avatar to Steve"
                  >
                    Reset
                  </Button>
                  <Button
                    variant="secondary"
                    size="lg"
                    icon="edit"
                    disabled={deleteKey === selectedSkin.texture_key}
                    onClick={() => void startEdit(selectedSkin)}
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
                onClick={() => void saveProfileSkin()}
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
              <Button variant="secondary" icon="plus" disabled={!canUpload} onClick={() => openUploadPicker(false)}>
                Add skin
              </Button>
            )}
          </div>
        )}
      </section>

      <section
        ref={savedSkinsDropSurfaceRef}
        class="cp-skinwork"
        data-saved-skins-drop-surface
        data-saved-skins-drop-state={uploadDragActive ? 'active' : 'idle'}
        onDragEnter={handleUploadDragEnter}
        onDragOver={handleUploadDragOver}
        onDragLeave={handleUploadDragLeave}
        onDrop={handleUploadDrop}
      >
        <div class="cp-skin-find" role="search" aria-label="Find player skin">
          <Input
            value={lookupUsername}
            onChange={(value) => {
              setLookupUsername(clampPlayerNameInput(value));
              setLookupVariant('classic');
              setLookupProfile(null);
              setLookupState('idle');
              setLookupError(null);
              setMessage(null);
              setPreviewExtra((current) => (current?.kind === 'lookup' ? null : current));
            }}
            onKeyDown={(event) => {
              if (event.key === 'Enter' && canLookupSkin) void lookupSkin();
            }}
            placeholder="Find a player's skin by username"
            icon="search"
            style={{ flex: '1 1 240px', minWidth: 0 }}
          />
          <Button
            variant="secondary"
            icon={lookupBusy && lookupState === 'loading' ? 'refresh' : 'search'}
            disabled={!canLookupSkin}
            onClick={() => void lookupSkin()}
            title={lookupUsernameError || 'Look up this player skin'}
          >
            {lookupState === 'loading' ? 'Searching' : 'Search'}
          </Button>
        </div>

        {lookupUsernameError && lookupState !== 'error' && (
          <div class="cp-skin-inline-err">{lookupUsernameError}</div>
        )}
        {lookupState === 'error' && lookupError && (
          <div class="cp-skin-inline-err">{lookupError}</div>
        )}
        {message && message.tone === 'err' && (
          <div class="cp-skin-inline-err">{message.text}</div>
        )}
        {state === 'unavailable' && (
          <div class="cp-skin-inline-err">{error || 'Saved skins are unavailable.'}</div>
        )}

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
                onClick={() => openUploadPicker(false)}
                title="Upload a PNG skin file, or drop one here"
              >
                <Icon name="plus" size={24} />
                <span class="cp-skin-add-tile__label">Add skin</span>
                <span class="cp-skin-add-tile__hint">
                  {uploadDragActive ? 'Drop to add' : 'Drag and drop'}
                </span>
              </button>

              {minecraftProfile && profileSkin && !showProfileSelectedPreview && (
                <div class="cp-skin-tilewrap">
                  <button
                    type="button"
                    class="cp-skin-tile"
                    data-kind="profile"
                    data-selected={profilePreviewActive ? 'true' : 'false'}
                    aria-pressed={profilePreviewActive}
                    onClick={viewProfileSkin}
                    onContextMenu={(event) => openContextMenu(event, profileMenuItems)}
                    title="Preview the current Minecraft profile skin"
                  >
                    <SkinSnapshotImg
                      cacheKey={`profile:${minecraftProfile.id}:${profileSkinVariant}:${profileCape?.id ?? ''}`}
                      src={profileSkinFileSrc ?? apiResourceUrl('/skin/profile/file')}
                      variant={profileSkinVariant}
                      capeSrc={profileCape ? capeFileUrl(profileCape) : undefined}
                      alt={`${minecraftProfile.name} current profile skin`}
                    />
                    <span class="cp-skin-tile__label">Current profile</span>
                  </button>
                </div>
              )}

              {librarySkins.map((skin) => {
                const applied = Boolean(skin.applied_at);
                const selected = !previewExtra && selectedSkin?.texture_key === skin.texture_key;
                const queued = pendingApplyKey === skin.texture_key;
                const deleting = deleteKey === skin.texture_key;
                const applyBlocked = applyKey === skin.texture_key || flushBusy || cancelPendingBusy;
                const pendingRowActionBusy = flushBusy || cancelPendingBusy || applyKey !== null;
                const tileMenuItems = menuItemsForSavedSkin({
                  skin,
                  selectedPreviewEditing: editKey === skin.texture_key,
                  onlineReady,
                  applying: applyBlocked,
                  pendingActionBusy: pendingRowActionBusy,
                  queued,
                  deleting,
                  onView: () => void viewSavedSkin(skin.texture_key),
                  onApply: () => void applySkin(skin.texture_key),
                  onApplyNow: () => void flushPendingApply(),
                  onCancelQueue: () => void cancelPendingApply(),
                  onEdit: () => void startEdit(skin),
                  onDownload: () => void downloadSavedSkin(skin),
                  onDelete: () => void confirmDeleteSkin(skin),
                });

                return (
                  <div class="cp-skin-tilewrap" key={skin.texture_key}>
                    <button
                      type="button"
                      class="cp-skin-tile"
                      data-selected={selected ? 'true' : 'false'}
                      aria-pressed={selected}
                      disabled={deleting}
                      onClick={() => void viewSavedSkin(skin.texture_key)}
                      onContextMenu={tileMenuItems.length === 0
                        ? undefined
                        : (event) => openContextMenu(event, tileMenuItems)}
                      title={skin.name}
                    >
                      <SkinSnapshotImg
                        cacheKey={`${skin.texture_key}:${skin.variant}:${skin.cape_id ?? ''}`}
                        src={savedSkinFileUrl(skin)}
                        variant={skin.variant}
                        capeSrc={capeSrcForId(skin.cape_id)}
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
                        disabled={tileMenuItems.length === 0}
                        onClick={(event) => {
                          event.stopPropagation();
                          openContextMenu(event, tileMenuItems);
                        }}
                      />
                    </span>
                  </div>
                );
              })}
            </div>
          )}
        </section>

        <section class="cp-skin-section" aria-label="Default skins">
          <header class="cp-skin-section__head">
            <span class="cp-skin-section__title">Default skins</span>
            <span class="cp-skin-section__hint">Always available</span>
          </header>
          <div class="cp-skin-strip">
            {DEFAULT_SKINS.map((skin) => {
              const savedRecord = savedRecordForDefault(skin.id);
              const selected = selectedDefault?.id === skin.id
                || Boolean(!previewExtra && savedRecord && selectedSkin?.texture_key === savedRecord.texture_key);
              const queued = Boolean(savedRecord && pendingApplyKey === savedRecord.texture_key);
              const applied = Boolean(savedRecord?.applied_at);
              return (
                <button
                  type="button"
                  key={skin.id}
                  class="cp-skin-tile cp-skin-tile--compact"
                  data-selected={selected ? 'true' : 'false'}
                  aria-pressed={selected}
                  onClick={() => viewDefaultSkin(skin.id)}
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
            })}
          </div>
        </section>

        {availableCapes.length > 0 && selectedSkin && (
          <section class="cp-skin-section" aria-label="Capes">
            <header class="cp-skin-section__head">
              <span class="cp-skin-section__title">Capes</span>
              <span class="cp-skin-section__hint">
                {capeBusy ? 'Updating cape...' : `Worn with ${selectedSkin.name}`}
              </span>
            </header>
            <CapePicker
              capes={availableCapes}
              value={selectedSkin.cape_id ?? NO_CAPE_VALUE}
              onChange={(value) => void changeSelectedSkinCape(value)}
            />
          </section>
        )}
      </section>

      <Modal
        open={Boolean(stagedUpload)}
        onOpenChange={(next) => {
          if (!next && !busy) clearStagedUpload();
        }}
      >
        <ModalContent className="cp-skinedit-modal" aria-label="Add skin" aria-describedby={undefined}>
          <ModalHeader>
            <ModalTitle>Add skin</ModalTitle>
          </ModalHeader>
          {stagedUpload && stagedVariant && (
            <div class="cp-skinedit">
              <div class="cp-skinedit__preview">
                <SkinThreePreview
                  src={stagedSkinPreviewSrc(stagedUpload)}
                  capeSrc={stagedCapeSrc}
                  name={stagedName}
                  variant={stagedVariant}
                  side="front"
                  showOuterLayers
                />
              </div>
              <div class="cp-skinedit__form">
                <label class="cp-skinedit__field">
                  <span>Name</span>
                  <Input
                    value={skinName}
                    onChange={(value) => {
                      setSkinName(value.slice(0, 64));
                      setMessage(null);
                    }}
                    placeholder={uploadSkinName(stagedUpload.file) || 'Skin name'}
                  />
                </label>
                <div class="cp-skinedit__field">
                  <span>Model</span>
                  <Segmented<UploadSkinVariant>
                    options={[
                      { value: 'auto', label: 'Auto' },
                      { value: 'classic', label: 'Classic' },
                      { value: 'slim', label: 'Slim' },
                    ]}
                    value={uploadVariant}
                    onChange={(value) => {
                      setUploadVariant(value);
                      setMessage(null);
                    }}
                  />
                </div>
                {availableCapes.length > 0 && (
                  <div class="cp-skinedit__field">
                    <span>Cape</span>
                    <CapePicker
                      capes={availableCapes}
                      value={stagedCapeId}
                      onChange={setStagedCapeId}
                    />
                  </div>
                )}
                {stagedUpload.normalizeStatus === 'error' ? (
                  <div class="cp-skin-inline-err">
                    {stagedUpload.normalizeError || 'This file is not a valid Minecraft skin.'}
                  </div>
                ) : stagedUpload.normalizeStatus === 'checking' ? (
                  <div class="cp-skinedit__meta">Checking skin file...</div>
                ) : null}
              </div>
              <div class="cp-skinedit__footer">
                <Button variant="ghost" disabled={busy} onClick={clearStagedUpload}>
                  Cancel
                </Button>
                <Button
                  variant="secondary"
                  icon={busy ? 'refresh' : 'download'}
                  disabled={!stagedCanSave}
                  onClick={() => saveStagedUpload(false)}
                >
                  Save
                </Button>
                <Button
                  variant="primary"
                  icon={busy ? 'refresh' : 'check'}
                  disabled={!stagedCanSave || !onlineReady}
                  onClick={() => saveStagedUpload(true)}
                  title={onlineReady ? 'Save locally, then apply to the active Minecraft account' : 'Online Minecraft account required'}
                  sound="affirm"
                >
                  Save & apply
                </Button>
              </div>
            </div>
          )}
        </ModalContent>
      </Modal>

      <Modal
        open={Boolean(editingSkin)}
        onOpenChange={(next) => {
          if (!next && editBusyKey === null) cancelEdit();
        }}
      >
        <ModalContent className="cp-skinedit-modal" aria-label="Edit skin" aria-describedby={undefined}>
          <ModalHeader>
            <ModalTitle>Edit skin</ModalTitle>
          </ModalHeader>
          {editingSkin && (
            <div
              class="cp-skinedit"
              data-saved-skin-edit-drop-surface={editingSkin.texture_key}
              data-saved-skin-edit-drop-state={editReplacementDragActive ? 'active' : 'idle'}
              onDragEnter={handleEditReplacementDragEnter}
              onDragOver={handleEditReplacementDragOver}
              onDragLeave={handleEditReplacementDragLeave}
              onDrop={handleEditReplacementDrop}
            >
              <div class="cp-skinedit__preview">
                <SkinThreePreview
                  src={editReplacement ? stagedSkinPreviewSrc(editReplacement) : savedSkinFileUrl(editingSkin)}
                  capeSrc={editPreviewCapeSrc}
                  name={trimmedEditName || editingSkin.name}
                  variant={editVariant}
                  side="front"
                  showOuterLayers
                />
              </div>
              <div class="cp-skinedit__form">
                <label class="cp-skinedit__field">
                  <span>Name</span>
                  <Input
                    value={editName}
                    onChange={(value) => setEditName(value.slice(0, 64))}
                    placeholder="Skin name"
                  />
                </label>
                <div class="cp-skinedit__field">
                  <span>Model</span>
                  <div class="cp-skinedit__inline">
                    <Segmented<SkinVariant>
                      options={[
                        { value: 'classic', label: 'Classic' },
                        { value: 'slim', label: 'Slim' },
                      ]}
                      value={editVariant}
                      onChange={(value) => {
                        setEditVariant(value);
                        setEditDetectError(null);
                      }}
                    />
                    <Button
                      variant="ghost"
                      size="sm"
                      icon={editDetectBusyKey === editingSkin.texture_key ? 'refresh' : 'search'}
                      disabled={editBusyKey !== null || editDetectBusyKey !== null}
                      onClick={() => void detectSavedSkinModel(editingSkin)}
                      title="Detect the model from the skin texture"
                    >
                      Detect
                    </Button>
                  </div>
                </div>
                {availableCapes.length > 0 && (
                  <div class="cp-skinedit__field">
                    <span>Cape</span>
                    <CapePicker
                      capes={availableCapes}
                      value={editCapeId}
                      onChange={setEditCapeId}
                    />
                  </div>
                )}
                <div class="cp-skinedit__field">
                  <span>Texture</span>
                  <div class="cp-skinedit__inline">
                    <Button
                      variant="secondary"
                      size="sm"
                      icon="image"
                      disabled={editBusyKey !== null || editDetectBusyKey !== null}
                      onClick={openEditTexturePicker}
                      title="Replace this skin PNG, or drop a file on this panel"
                    >
                      Replace PNG
                    </Button>
                    {editReplacement && (
                      <>
                        <span class="cp-skinedit__meta">
                          {editReplacement.normalizeStatus === 'error'
                            ? editReplacement.normalizeError || 'Skin validation failed.'
                            : editReplacement.normalizeStatus === 'checking'
                              ? 'Validating...'
                              : 'Replacement ready'}
                        </span>
                        <Button
                          variant="ghost"
                          size="sm"
                          icon="x"
                          disabled={editBusyKey !== null}
                          onClick={clearEditReplacement}
                        >
                          Remove
                        </Button>
                      </>
                    )}
                  </div>
                </div>
                {editDetectError && <div class="cp-skin-inline-err">{editDetectError}</div>}
              </div>
              <div class="cp-skinedit__footer">
                {!editingSkin.applied_at && (
                  <Button
                    variant="ghost"
                    icon="trash"
                    disabled={editBusyKey !== null || deleteKey === editingSkin.texture_key}
                    onClick={() => void confirmDeleteSkin(editingSkin)}
                    style={{ marginRight: 'auto' }}
                  >
                    Delete
                  </Button>
                )}
                <Button variant="ghost" disabled={editBusyKey !== null} onClick={cancelEdit}>
                  Cancel
                </Button>
                <Button
                  variant="secondary"
                  icon={editBusyKey === editingSkin.texture_key ? 'refresh' : 'download'}
                  disabled={
                    editBusyKey !== null ||
                    editDetectBusyKey !== null ||
                    !editReplacementReady ||
                    !savedSkinEditHasChanges(editingSkin) ||
                    trimmedEditName.length === 0
                  }
                  onClick={() => void saveSkinMetadata(editingSkin.texture_key)}
                >
                  Save
                </Button>
                <Button
                  variant="primary"
                  icon={editBusyKey === editingSkin.texture_key ? 'refresh' : 'check'}
                  disabled={
                    editBusyKey !== null ||
                    editDetectBusyKey !== null ||
                    !editReplacementReady ||
                    !savedSkinEditHasChanges(editingSkin) ||
                    trimmedEditName.length === 0 ||
                    !onlineReady
                  }
                  onClick={() => void saveSkinMetadata(editingSkin.texture_key, true)}
                  title={onlineReady
                    ? 'Save changes, then apply to the active Minecraft account'
                    : 'Online Minecraft account required'}
                  sound="affirm"
                >
                  Save & apply
                </Button>
              </div>
            </div>
          )}
        </ModalContent>
      </Modal>
    </div>
  );
}

function SkinSnapshotImg({
  cacheKey,
  src,
  variant,
  capeSrc,
  alt,
}: {
  cacheKey: string;
  src: string;
  variant: SkinVariant;
  capeSrc?: string;
  alt: string;
}): JSX.Element {
  const [url, setUrl] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    setUrl(null);
    skinSnapshot(cacheKey, src, variant, capeSrc)
      .then((nextUrl) => {
        if (active) setUrl(nextUrl);
      })
      .catch(() => {
        if (active) setUrl(null);
      });
    return () => {
      active = false;
    };
  }, [cacheKey, src, variant, capeSrc]);

  if (!url) {
    return <span class="cp-skin-tile__img" data-snapshot="loading" aria-hidden="true" />;
  }
  return <img class="cp-skin-tile__img" src={url} alt={alt} draggable={false} />;
}
