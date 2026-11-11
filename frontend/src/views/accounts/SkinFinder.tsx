import type { JSX } from 'preact';
import { IconButton, Input, Ring } from '../../ui/Atoms';

/** Try on any player's skin by username. Once one is on the stage, the field is also where it is let go. */
export function SkinFinder({
  username,
  busy,
  previewing,
  invalid,
  onUsernameChange,
  onLookup,
  onDismiss,
}: {
  username: string;
  busy: boolean;
  previewing: boolean;
  invalid: boolean;
  onUsernameChange: (value: string) => void;
  onLookup: () => void;
  onDismiss: () => void;
}): JSX.Element {
  return (
    <div role="search" aria-label="Find a player's skin">
      <Input
        value={username}
        onChange={onUsernameChange}
        onKeyDown={(event) => {
          if (event.key === 'Enter') onLookup();
        }}
        placeholder="Try on a player's skin by username"
        ariaLabel="Player username"
        icon="search"
        invalid={invalid}
        trailing={
          busy ? (
            <Ring size={14} />
          ) : (
            previewing && <IconButton icon="x" size={24} tooltip="Stop previewing this player" onClick={onDismiss} />
          )
        }
      />
    </div>
  );
}
