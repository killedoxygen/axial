import type { JSX } from 'preact';

const PLACEHOLDER_SAVES = [
  { name: 'Survival World', lastPlayed: '2 hours ago', size: '142 MB', icon: 'grass' },
  { name: 'Creative Build', lastPlayed: '3 days ago', size: '87 MB', icon: 'diamond' },
  { name: 'Redstone Lab', lastPlayed: '1 week ago', size: '34 MB', icon: 'redstone' },
  { name: 'SMP Backup', lastPlayed: '2 weeks ago', size: '256 MB', icon: 'chest' },
];

function SaveIcon({ type }: { type: string }): JSX.Element {
  if (type === 'diamond') return <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M6 5h12l3 5l-8.5 9.5a.7.7 0 0 1-1 0L3 10l3-5" /><path d="M10 5l-2 5l4 4.5" /><path d="M14 5l2 5l-4 4.5" /></svg>;
  if (type === 'redstone') return <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M13 3l0 7l6 0l-8 11l0-7l-6 0l8-11" /></svg>;
  if (type === 'chest') return <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7v11a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2V7" /><path d="M3 7h18l-2-4H5L3 7z" /><path d="M10 12h4" /></svg>;
  return <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12a9 9 0 1 0 18 0a9 9 0 0 0-18 0" /><path d="M3.6 9h16.8" /><path d="M3.6 15h16.8" /><path d="M11.5 3a17 17 0 0 0 0 18" /><path d="M12.5 3a17 17 0 0 1 0 18" /></svg>;
}

export function SavesPane({ count }: { count: number }): JSX.Element {
  return (
    <div class="mock-pane">
      <div class="mock-header">
        <div class="mock-header-title">
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12a9 9 0 1 0 18 0a9 9 0 0 0-18 0" /><path d="M3.6 9h16.8" /><path d="M3.6 15h16.8" /><path d="M11.5 3a17 17 0 0 0 0 18" /><path d="M12.5 3a17 17 0 0 1 0 18" /></svg>
          {count} world{count === 1 ? '' : 's'}
        </div>
        <div class="mock-badge">Coming soon</div>
      </div>
      <div class="mock-list">
        {PLACEHOLDER_SAVES.map((save) => (
          <div class="mock-list-item" key={save.name}>
            <div class="mock-list-icon"><SaveIcon type={save.icon} /></div>
            <div class="mock-list-info">
              <div class="mock-list-name">{save.name}</div>
              <div class="mock-list-meta">{save.lastPlayed} &middot; {save.size}</div>
            </div>
            <div class="mock-list-actions">
              <div class="mock-btn-sm">Backup</div>
              <div class="mock-btn-sm">Open</div>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}
