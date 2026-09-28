import type { JSX } from 'preact';
import { CapePicker } from './CapePicker';
import { NO_CAPE_VALUE, type MinecraftCape, type SavedSkinRecord } from './types';

export function SavedSkinCapeSection({
  availableCapes,
  selectedSkin,
  capeBusy,
  onChange,
}: {
  availableCapes: MinecraftCape[];
  selectedSkin: SavedSkinRecord;
  capeBusy: boolean;
  onChange: (value: string) => void;
}): JSX.Element {
  return (
    <section class="cp-skin-section" aria-label="Capes">
      <header class="cp-skin-section__head">
        <span class="cp-skin-section__title">Capes</span>
        <span class="cp-skin-section__hint">{capeBusy ? 'Updating cape...' : `Worn with ${selectedSkin.name}`}</span>
      </header>
      <CapePicker capes={availableCapes} value={selectedSkin.cape_id ?? NO_CAPE_VALUE} onChange={onChange} />
    </section>
  );
}
