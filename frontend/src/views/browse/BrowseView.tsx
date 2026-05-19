import type { JSX } from 'preact';
import { Card } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';

export function BrowseView(): JSX.Element {
  return (
    <div class="cp-view-page" style={{ gap: 20 }}>
      <div class="cp-page-header">
        <div>
          <h1>Browse</h1>
          <div class="cp-page-sub">Mod discovery coming soon</div>
        </div>
      </div>
      <Card padding={32}>
        <div class="cp-empty">
          <Icon name="compass" size={36} color="var(--text-mute)" />
          <h2>Nothing to browse yet</h2>
          <p>The mod catalog experience is still being built. In the meantime, drop mods into an instance's folder and they'll show up on its Mods tab.</p>
        </div>
      </Card>
    </div>
  );
}
