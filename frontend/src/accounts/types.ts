import type { SkinVariant } from '../generated/SkinVariant';

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
