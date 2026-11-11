import type { JSX } from 'preact';
import { SettingRow, SettingsSection } from '../../ui/SettingsSheet';
import { AccentField, ThemeChoice } from './AccentEditor';

export function AppearanceSection(): JSX.Element {
  return (
    <SettingsSection>
      <SettingRow title="Theme">
        <ThemeChoice />
      </SettingRow>
      <SettingRow title="Accent">
        <AccentField />
      </SettingRow>
    </SettingsSection>
  );
}
