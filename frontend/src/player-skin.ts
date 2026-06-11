import { signal } from '@preact/signals';
import { apiResourceUrl } from './api';
import { DEFAULT_SKINS } from './default-skins';
import { local, saveLocalState } from './state';

export const accountSkinSrc = signal<string | null>(null);

export function selectedSkinTextureSrc(value = local.selectedSkin): string | null {
  if (value.startsWith('default:')) {
    const id = value.slice('default:'.length);
    return DEFAULT_SKINS.find((skin) => skin.id === id)?.src ?? null;
  }
  if (value.startsWith('saved:')) {
    const textureKey = value.slice('saved:'.length);
    return textureKey ? apiResourceUrl(`/skins/${textureKey}/file`) : null;
  }
  return null;
}

export function setSelectedSkin(value: string): void {
  if (local.selectedSkin !== value) {
    local.selectedSkin = value;
    saveLocalState();
  }
  accountSkinSrc.value = selectedSkinTextureSrc();
}

export function resetSelectedSkin(): void {
  setSelectedSkin('default:steve');
}

export function refreshAccountSkin(): void {
  accountSkinSrc.value = selectedSkinTextureSrc();
}
