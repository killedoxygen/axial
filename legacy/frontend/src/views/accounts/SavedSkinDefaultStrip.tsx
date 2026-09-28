import type { JSX } from 'preact';
import type { DefaultSkin } from '../../default-skins';
import { DefaultSkinTile } from './SkinTiles';
import type { SavedSkinRecord } from './types';

export function SavedSkinDefaultStrip({
  skins,
  selectedDefaultId,
  selectedSkinTextureKey,
  previewExtraActive,
  pendingApplyKey,
  skinActionsEnabled,
  currentProfileSavedKey,
  savedRecordForDefault,
  onViewDefaultSkin,
}: {
  skins: DefaultSkin[];
  selectedDefaultId?: string | null;
  selectedSkinTextureKey?: string | null;
  previewExtraActive: boolean;
  pendingApplyKey: string | null;
  skinActionsEnabled: boolean;
  currentProfileSavedKey: string | null;
  savedRecordForDefault: (id: string) => SavedSkinRecord | null;
  onViewDefaultSkin: (id: string) => void;
}): JSX.Element {
  return (
    <section class="cp-skin-section" aria-label="Default skins">
      <header class="cp-skin-section__head">
        <span class="cp-skin-section__title">Default skins</span>
        <span class="cp-skin-section__hint">Always available</span>
      </header>
      <div class="cp-skin-strip">
        {skins.map((skin) => {
          const savedRecord = savedRecordForDefault(skin.id);
          const selected =
            selectedDefaultId === skin.id ||
            Boolean(!previewExtraActive && savedRecord && selectedSkinTextureKey === savedRecord.texture_key);
          const queued = Boolean(savedRecord && pendingApplyKey === savedRecord.texture_key);
          const applied = Boolean(
            savedRecord?.applied_at ||
            (skinActionsEnabled && savedRecord && savedRecord.texture_key === currentProfileSavedKey),
          );
          return (
            <DefaultSkinTile
              key={skin.id}
              skin={skin}
              selected={selected}
              queued={queued}
              applied={applied}
              onView={() => onViewDefaultSkin(skin.id)}
            />
          );
        })}
      </div>
    </section>
  );
}
