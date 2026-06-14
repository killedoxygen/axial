import type { JSX } from 'preact';
import { Button } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { closeCreate, navigate } from '../../ui-state';

export function LibraryBlocker(): JSX.Element {
  return (
    <div class="cp-cr-blocker">
      <Icon name="folder" size={32} />
      <h2>Set up your library first</h2>
      <p>Croopor needs a place to keep game files before you can make an instance.</p>
      <Button
        icon="settings"
        onClick={() => {
          closeCreate();
          navigate({ name: 'settings' });
        }}
      >
        Open setup
      </Button>
    </div>
  );
}
