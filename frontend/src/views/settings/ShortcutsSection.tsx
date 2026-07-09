import type { JSX } from 'preact';
import { Kbd } from '../../ui/Atoms';
import { SettingRow, SettingsSection } from '../../ui/SettingsSheet';
import { SHORTCUTS } from '../../shortcuts';

export function ShortcutsSection(): JSX.Element {
  return (
    <SettingsSection>
      {SHORTCUTS.map((def) => (
        <SettingRow
          key={def.id}
          title={def.label}
          control={
            <span class="cp-settings-combos">
              {def.combos.map((combo) => (
                <span key={combo.join('+')} class="cp-settings-combo">
                  {combo.map((key) => (
                    <Kbd key={key}>{key}</Kbd>
                  ))}
                </span>
              ))}
            </span>
          }
        />
      ))}
    </SettingsSection>
  );
}
