import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Button, Keys } from '../../ui/Atoms';
import { FloatingTray, FloatingTrayLabel } from '../../ui/FloatingTray';
import { OverrideChip } from '../../ui/SettingsSheet';
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
      <div class="cp-shortcuts">
        {SHORTCUTS.map((def) => {
          const override = def.fixed ? null : shortcutOverride(def.id);
          const chips = effectiveCombos(def).map((combo) => (
            <Keys key={comboParts(combo).join('+')} parts={comboParts(combo)} />
          ));
          const isRecording = recording === def.id;
          return (
            <div key={def.id} class="cp-shortcut">
              {def.fixed ? (
                <span class="cp-shortcut-keys">{chips}</span>
              ) : (
                <button
                  type="button"
                  class="cp-shortcut-keys"
                  data-recording={isRecording}
                  aria-label={`Change the shortcut for ${def.label}`}
                  title="Click, then press the new key combo. Esc cancels."
                  onClick={() => setRecording(isRecording ? null : def.id)}
                >
                  {!isRecording ? (
                    chips
                  ) : preview.length > 0 ? (
                    <Keys parts={preview} />
                  ) : (
                    <span class="cp-shortcut-listening">Listening…</span>
                  )}
                </button>
              )}
              <span class="cp-shortcut-name">
                {def.label}
                {override && <OverrideChip label="Custom" onReset={() => setShortcutOverride(def.id, null)} />}
              </span>
            </div>
          );
        })}
      </div>
      {recordingDef && (
        <FloatingTray ariaLabel={`Recording a shortcut for ${recordingDef.label}`}>
          <FloatingTrayLabel>{recordingDef.label}</FloatingTrayLabel>
          <span class="cp-shortcut-hud-combo">
            {preview.length > 0 ? (
              <Keys parts={preview} />
            ) : (
              <span class="cp-shortcut-hud-hint">Press the new key combo</span>
            )}
          </span>
          <span class="cp-shortcut-hud-hint">Esc cancels</span>
          <Button variant="ghost" size="sm" onClick={() => setRecording(null)}>
            Cancel
          </Button>
        </FloatingTray>
      )}
    </>
  );
}
