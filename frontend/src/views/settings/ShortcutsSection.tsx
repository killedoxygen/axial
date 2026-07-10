import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Button, Kbd } from '../../ui/Atoms';
import { FloatingPill, FloatingPillDivider } from '../../ui/FloatingPill';
import { OverrideChip, SettingRow, SettingsSection } from '../../ui/SettingsSheet';
import {
  SHORTCUTS,
  captureCombo,
  comboParts,
  effectiveCombos,
  eventComboParts,
  findConflict,
  setShortcutOverride,
  shortcutById,
  shortcutOverride,
  shortcutsVersion,
  type ShortcutId,
} from '../../shortcuts';
import { toast } from '../../toast';

export function ShortcutsSection(): JSX.Element {
  shortcutsVersion.value;
  const [recording, setRecording] = useState<ShortcutId | null>(null);
  const [preview, setPreview] = useState<string[]>([]);

  useEffect(() => {
    if (!recording) return;
    setPreview([]);
    const onKeyDown = (e: KeyboardEvent): void => {
      e.preventDefault();
      e.stopPropagation();
      if (e.key === 'Escape') {
        setRecording(null);
        return;
      }
      setPreview(eventComboParts(e));
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
    const onKeyUp = (e: KeyboardEvent): void => {
      e.preventDefault();
      e.stopPropagation();
      setPreview(eventComboParts(e, false));
    };
    window.addEventListener('keydown', onKeyDown, true);
    window.addEventListener('keyup', onKeyUp, true);
    return () => {
      window.removeEventListener('keydown', onKeyDown, true);
      window.removeEventListener('keyup', onKeyUp, true);
    };
  }, [recording]);

  const recordingDef = recording ? shortcutById(recording) : null;

  return (
    <>
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
          const isRecording = recording === def.id;
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
                    data-recording={isRecording}
                    title="Click, then press the new key combo. Esc cancels."
                    onClick={() => setRecording(isRecording ? null : def.id)}
                  >
                    {isRecording ? (
                      preview.length > 0 ? (
                        <span class="cp-settings-combo">
                          {preview.map((part) => (
                            <Kbd key={part}>{part}</Kbd>
                          ))}
                        </span>
                      ) : (
                        <span class="cp-shortcut-listening">Listening…</span>
                      )
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
      {recordingDef && (
        <FloatingPill ariaLabel={`Recording a shortcut for ${recordingDef.label}`}>
          <span class="cp-shortcut-hud-label">{recordingDef.label}</span>
          <FloatingPillDivider />
          <span class="cp-shortcut-hud-combo">
            {preview.length > 0 ? (
              preview.map((part) => <Kbd key={part}>{part}</Kbd>)
            ) : (
              <span class="cp-shortcut-hud-hint">Press the new key combo</span>
            )}
          </span>
          <FloatingPillDivider />
          <span class="cp-shortcut-hud-hint">Esc cancels</span>
          <Button variant="ghost" size="sm" onClick={() => setRecording(null)}>
            Cancel
          </Button>
        </FloatingPill>
      )}
    </>
  );
}
