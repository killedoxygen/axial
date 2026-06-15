export interface MinecraftSkin {
  id: string;
  state: string;
  url: string;
  variant: string;
}

export interface MinecraftCape {
  id: string;
  state: string;
  url: string;
}

export interface MinecraftProfile {
  id: string;
  name: string;
  skins: MinecraftSkin[];
  capes: MinecraftCape[];
}

export interface MinecraftSkinLookup {
  username: string;
  uuid: string;
  source: string;
  variant: SkinVariant;
  texture_url: string;
  texture_file_url: string;
  cape_url: string | null;
  head_url: string;
}

export interface MinecraftAuthReadiness {
  minecraft_profile_ready?: boolean;
  minecraft_ownership_verified?: boolean;
  minecraft_profile?: MinecraftProfile;
  minecraft_token_expires_in?: number | null;
}

export interface AccountActionState {
  state_id: string;
  label: string;
  enabled: boolean;
  disabled_reason?: string;
}

export interface AuthStatus {
  launch_auth_mode: LaunchAuthMode;
  mode: string;
  username: string;
  uuid: string;
  provider: string;
  verified: boolean;
  online_mode_ready: boolean;
  skin_source: string;
  login_available: boolean;
  login_reason: string;
  msa_authenticated?: boolean;
  msa_provider?: string | null;
  msa_token_expires_in?: number | null;
  msa_refresh_available: boolean;
  skin_action?: AccountActionState;
}

export type AuthStatusRecord = AuthStatus & MinecraftAuthReadiness;
export type AuthStatusState = 'loading' | 'ready' | 'unavailable';
export type LauncherAccountKind = 'microsoft' | 'offline';
export type SkinVariant = 'classic' | 'slim';
export type UploadSkinVariant = SkinVariant | 'auto';
export type SavedSkinSort = 'recent' | 'name' | 'equipped' | 'source';

export const NO_CAPE_VALUE = '__none';

export interface SavedSkinRecord {
  texture_key: string;
  name: string;
  variant: SkinVariant;
  source: string;
  cape_id: string | null;
  created_at: string;
  updated_at: string;
  applied_at: string | null;
  byte_size: number;
}

export interface SavedSkinsData {
  skins: SavedSkinRecord[];
  pendingApplyKey: string | null;
}

export interface LauncherAccount extends MinecraftAuthReadiness {
  account_id: string;
  kind: LauncherAccountKind;
  display_name: string;
  active: boolean;
  login_id?: string;
  minecraft_profile_id?: string;
  offline_uuid?: string;
  msa_authenticated: boolean;
  msa_token_expires_in?: number | null;
  msa_refresh_available: boolean;
}

export interface LauncherAccountsData {
  active_account_id: string | null;
  accounts: LauncherAccount[];
}

export interface SkinFlushResult {
  status: string;
  applied: number;
}

export interface StagedSkinUpload {
  file: File;
  objectUrl: string;
  normalizedDataUrl?: string;
  detectedVariant: SkinVariant;
  detectingVariant: boolean;
  normalizeStatus: 'checking' | 'ready' | 'error';
  normalizeError?: string;
  textureKey?: string;
  originalWidth?: number;
  originalHeight?: number;
  normalizedByteSize?: number;
  applyAfterSave: boolean;
}

export interface SkinNormalizeMetadata {
  textureKey: string;
  variantSuggestion: SkinVariant;
  originalWidth: number;
  originalHeight: number;
  normalizedByteSize: number;
  normalizedDataUrl?: string;
}
import type { LaunchAuthMode } from '../../types';
