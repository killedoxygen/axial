import type { JSX } from 'preact';
import { SettingRow, SettingsSection } from '../../ui/SettingsSheet';
import { AccentField, AccentModeToggle } from './AccentEditor';

export function AppearanceSection(): JSX.Element {
  return (
    <SettingsSection>
      <SettingRow
        title="Mode"
        description="Light or dark canvas. Accent colors re-derive automatically so contrast stays safe."
        control={<AccentModeToggle />}
      />
      <SettingRow
        title="Accent"
        description="Drag inside the field to pick any hue and chroma, or tap a preset. Every tint, ring, and on-accent contrast is derived from this single point."
      >
        <AccentField />
      </SettingRow>
    </SettingsSection>
  );
}
