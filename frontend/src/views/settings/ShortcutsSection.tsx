import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Kbd } from '../../ui/Atoms';
import { OverrideChip, SettingRow, SettingsSection } from '../../ui/SettingsSheet';
import {
  SHORTCUTS,
  captureCombo,
  comboParts,
  effectiveCombos,
  findConflict,
  setShortcutOverride,
  shortcutOverride,
  shortcutsVersion,
  type ShortcutId,
} from '../../shortcuts';
import { toast } from '../../toast';

export function ShortcutsSection(): JSX.Element {
  shortcutsVersion.value;
  const [recording, setRecording] = useState<ShortcutId | null>(null);

  useEffect(() => {
    if (!recording) return;
    const onKeyDown = (e: KeyboardEvent): void => {
      e.preventDefault();
      e.stopPropagation();
      if (e.key === 'Escape') {
        setRecording(null);
        return;
      }
      const combo = captureCombo(e);
      if (!combo) return;
      const conflict = findConflict(recording, combo);
      if (conflict) {
        toast(`That combo is already used by "${conflict.label}"`, 'error');
        return;
      }
      setShortcutOverride(recording, combo);
      setRecording(null);
      toast('Saved');
    };
    window.addEventListener('keydown', onKeyDown, true);
    return () => window.removeEventListener('keydown', onKeyDown, true);
  }, [recording]);

  return (
    <SettingsSection>
      {SHORTCUTS.map((def) => {
        const override = def.fixed ? null : shortcutOverride(def.id);
        const combos = effectiveCombos(def);
        const chips = combos.map((combo) => (
          <span key={comboParts(combo).join('+')} class="cp-settings-combo">
            {comboParts(combo).map((part) => (
              <Kbd key={part}>{part}</Kbd>
            ))}
          </span>
        ));
        return (
          <SettingRow
            key={def.id}
            title={def.label}
            aside={override && <OverrideChip label="Custom" onReset={() => setShortcutOverride(def.id, null)} />}
            control={
              def.fixed ? (
                <span class="cp-settings-combos">{chips}</span>
              ) : (
                <button
                  type="button"
                  class="cp-shortcut-edit"
                  data-recording={recording === def.id}
                  title="Click, then press the new key combo. Esc cancels."
                  onClick={() => setRecording(recording === def.id ? null : def.id)}
                >
                  {recording === def.id ? (
                    <span class="cp-shortcut-listening">Press keys…</span>
                  ) : (
                    <span class="cp-settings-combos">{chips}</span>
                  )}
                </button>
              )
            }
          />
        );
      })}
    </SettingsSection>
  );
}
