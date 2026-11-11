import type { SkinVariant } from '../../generated/SkinVariant';

export type { AccountActionState } from '../../generated/AccountActionState';
export type { AccountListResponse } from '../../generated/AccountListResponse';
export type { AccountView as LauncherAccount } from '../../generated/AccountView';
export type { MinecraftCape } from '../../generated/MinecraftCape';
export type { MinecraftProfile } from '../../generated/MinecraftProfile';
export type { PendingSkinStatus } from '../../generated/PendingSkinStatus';
export type { ProfileSkin } from '../../generated/ProfileSkin';
export type { SavedSkinRecord } from '../../generated/SavedSkinRecord';
export type { SavedSkinsResponse } from '../../generated/SavedSkinsResponse';
export type { SkinNormalizeResponse } from '../../generated/SkinNormalizeResponse';
export type { SkinVariant } from '../../generated/SkinVariant';
export type { UsernameProfile as MinecraftSkinLookup } from '../../generated/UsernameProfile';

export type UploadSkinVariant = SkinVariant | 'auto';
export type SavedSkinSort = 'recent' | 'name' | 'equipped' | 'source';

export const NO_CAPE_VALUE = '__none';

export interface StagedSkinUpload {
  file: File;
  objectUrl: string;
  normalizedDataUrl?: string;
  detectedVariant: SkinVariant;
  detectingVariant: boolean;
  normalizeStatus: 'checking' | 'ready' | 'error';
  normalizeError?: string;
}
