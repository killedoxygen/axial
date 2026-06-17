import type { JSX } from 'preact';
import { Button, Input } from '../../ui/Atoms';

export type SavedSkinLookupState = 'idle' | 'loading' | 'ready' | 'error';
export type SavedSkinLibraryState = 'loading' | 'ready' | 'unavailable';
export type SavedSkinLibraryMessage = { tone: 'ok' | 'err'; text: string };

export function SavedSkinLookupBar({
  lookupUsername,
  lookupBusy,
  lookupState,
  lookupUsernameError,
  lookupError,
  message,
  state,
  error,
  canLookupSkin,
  onLookupUsernameChange,
  onLookupSkin,
}: {
  lookupUsername: string;
  lookupBusy: boolean;
  lookupState: SavedSkinLookupState;
  lookupUsernameError: string | null;
  lookupError: string | null;
  message: SavedSkinLibraryMessage | null;
  state: SavedSkinLibraryState;
  error: string | null;
  canLookupSkin: boolean;
  onLookupUsernameChange: (value: string) => void;
  onLookupSkin: () => void;
}): JSX.Element {
  return (
    <>
      <div class="cp-skin-find" role="search" aria-label="Find player skin">
        <Input
          value={lookupUsername}
          onChange={onLookupUsernameChange}
          onKeyDown={(event) => {
            if (event.key === 'Enter' && canLookupSkin) onLookupSkin();
          }}
          placeholder="Find a player's skin by username"
          icon="search"
          style={{ flex: '1 1 240px', minWidth: 0 }}
        />
        <Button
          variant="secondary"
          icon={lookupBusy && lookupState === 'loading' ? 'refresh' : 'search'}
          disabled={!canLookupSkin}
          onClick={onLookupSkin}
          title={lookupUsernameError || 'Look up this player skin'}
        >
          {lookupState === 'loading' ? 'Searching' : 'Search'}
        </Button>
      </div>

      {lookupUsernameError && lookupState !== 'error' && <div class="cp-skin-inline-err">{lookupUsernameError}</div>}
      {lookupState === 'error' && lookupError && <div class="cp-skin-inline-err">{lookupError}</div>}
      {message && message.tone === 'err' && <div class="cp-skin-inline-err">{message.text}</div>}
      {state === 'unavailable' && <div class="cp-skin-inline-err">{error || 'Saved skins are unavailable.'}</div>}
    </>
  );
}
