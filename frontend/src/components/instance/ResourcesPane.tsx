import type { JSX } from 'preact';

const PLACEHOLDER_PACKS = [
  { name: 'Faithful 32x', res: '32x', desc: 'Classic faithful textures', active: true },
  { name: 'Stay True', res: '16x', desc: 'Connected textures and biome variants', active: true },
  { name: 'Vanilla Tweaks', res: '16x', desc: 'Customizable UI and quality of life changes', active: false },
];

export function ResourcesPane({ count }: { count: number }): JSX.Element {
  return (
    <div class="mock-pane">
      <div class="mock-header">
        <div class="mock-header-title">
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 21a9 9 0 0 1 0-18c4.97 0 9 3.582 9 8c0 1.06-.474 2.078-1.318 2.828a4.007 4.007 0 0 1-2.682 1.172h-2.5a2 2 0 0 0-1 3.75a1.3 1.3 0 0 1-1.5 1.25" /><path d="M8.5 10.5m-1 0a1 1 0 1 0 2 0a1 1 0 1 0-2 0" /><path d="M12.5 7.5m-1 0a1 1 0 1 0 2 0a1 1 0 1 0-2 0" /><path d="M16.5 10.5m-1 0a1 1 0 1 0 2 0a1 1 0 1 0-2 0" /></svg>
          {count} pack{count === 1 ? '' : 's'}
        </div>
        <div class="mock-badge">Coming soon</div>
      </div>
      <div class="mock-list">
        {PLACEHOLDER_PACKS.map((pack) => (
          <div class={`mock-list-item${!pack.active ? ' mock-disabled' : ''}`} key={pack.name}>
            <div class="mock-list-icon mock-list-icon-square">
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 21a9 9 0 0 1 0-18c4.97 0 9 3.582 9 8c0 1.06-.474 2.078-1.318 2.828a4.007 4.007 0 0 1-2.682 1.172h-2.5a2 2 0 0 0-1 3.75a1.3 1.3 0 0 1-1.5 1.25" /><path d="M8.5 10.5m-1 0a1 1 0 1 0 2 0a1 1 0 1 0-2 0" /><path d="M12.5 7.5m-1 0a1 1 0 1 0 2 0a1 1 0 1 0-2 0" /><path d="M16.5 10.5m-1 0a1 1 0 1 0 2 0a1 1 0 1 0-2 0" /></svg>
            </div>
            <div class="mock-list-info">
              <div class="mock-list-name">{pack.name} <span class="mock-list-version">{pack.res}</span></div>
              <div class="mock-list-meta">{pack.desc}</div>
            </div>
            <div class="mock-list-actions">
              <div class="mock-btn-sm">{pack.active ? 'Disable' : 'Enable'}</div>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}
