import type { JSX } from 'preact';
import { Button, Toggle } from '../../ui/Atoms';
import { AccentField } from '../settings/AccentEditor';
import type { Preferences } from './finish';
import type { Player } from './use-identity';
import { Step } from './Step';

const PREFERENCES: { key: keyof Preferences; label: string; detail: string }[] = [
  { key: 'music', label: 'Ambient music', detail: 'Pauses itself when the game starts.' },
  { key: 'discord', label: 'Discord activity', detail: 'Shows that you are playing, never where.' },
  {
    key: 'telemetry',
    label: 'Anonymous usage stats',
    detail: 'Feature use and launch success. Never your name, files or servers.',
  },
];

export function Style({
  player,
  preferences,
  onChange,
  onBack,
  saving,
  error,
}: {
  player: Player | null;
  preferences: Preferences;
  onChange: (preferences: Preferences) => void;
  onBack: () => void;
  saving: boolean;
  error: string | null;
}): JSX.Element {
  return (
    <Step
      index={1}
      view="style"
      title="Make it yours."
      lead="Pick a color, then choose what Axial does for you."
      player={player}
      aside={
        <Button variant="ghost" size="lg" disabled={saving} onClick={onBack}>
          Back
        </Button>
      }
      next={{ label: saving ? 'Starting…' : 'Start playing', disabled: saving }}
      error={error}
    >
      <AccentField compact />
      <ul class="cp-ob-card cp-ob-prefs">
        {PREFERENCES.map(({ key, label, detail }) => (
          <li key={key}>
            <div class="cp-ob-card-text">
              <span class="cp-ob-card-title">{label}</span>
              <span class="cp-ob-note">{detail}</span>
            </div>
            <Toggle
              on={preferences[key]}
              ariaLabel={label}
              disabled={saving}
              onChange={() => onChange({ ...preferences, [key]: !preferences[key] })}
            />
          </li>
        ))}
      </ul>
    </Step>
  );
}
