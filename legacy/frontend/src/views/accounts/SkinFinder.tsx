import type { JSX } from 'preact';
import { Icon } from '../../ui/Icons';
import { Input } from '../../ui/Atoms';

export function SkinFinder({
  username,
  busy,
  canLookup,
  usernameError,
  onUsernameChange,
  onLookup,
}: {
  username: string;
  busy: boolean;
  canLookup: boolean;
  usernameError: string | null;
  onUsernameChange: (value: string) => void;
  onLookup: () => void;
}): JSX.Element {
  const trimmed = username.trim();
  return (
    <div class="cp-skinfinder" role="search" aria-label="Find player skin">
      <Input
        value={username}
        onChange={onUsernameChange}
        onKeyDown={(event) => {
          if (event.key === 'Enter' && canLookup) onLookup();
        }}
        placeholder="Fetch any player's skin by username"
        icon="search"
        trailing={
          busy ? (
            <span class="cp-skinfinder__wait" aria-hidden="true">
              <Icon name="refresh" size={14} />
            </span>
          ) : trimmed ? (
            <button
              type="button"
              class="cp-skinfinder__go"
              disabled={!canLookup}
              title={usernameError || 'Preview this player skin on the stage'}
              onClick={onLookup}
            >
              <span>Preview</span>
              <kbd>Enter</kbd>
            </button>
          ) : (
            <span class="cp-skinfinder__hint">Previews on the stage, saves to your library</span>
          )
        }
      />
    </div>
  );
}
