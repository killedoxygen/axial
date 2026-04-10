import type { JSX } from 'preact';

const PLACEHOLDER_SCREENSHOTS = [
  { date: 'Today, 14:32', dims: '1920x1080' },
  { date: 'Today, 12:07', dims: '1920x1080' },
  { date: 'Yesterday, 21:45', dims: '1920x1080' },
  { date: '3 days ago', dims: '2560x1440' },
  { date: '1 week ago', dims: '1920x1080' },
  { date: '1 week ago', dims: '1920x1080' },
];

export function ScreenshotsPane(): JSX.Element {
  return (
    <div class="mock-pane">
      <div class="mock-header">
        <div class="mock-header-title">
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M5 7h1a2 2 0 0 0 2-2a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1a2 2 0 0 0 2 2h1a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2h-14a2 2 0 0 1-2-2v-9a2 2 0 0 1 2-2" /><path d="M9 13a3 3 0 1 0 6 0a3 3 0 0 0-6 0" /></svg>
          Screenshots
        </div>
        <div class="mock-badge">Coming soon</div>
      </div>
      <div class="mock-gallery">
        {PLACEHOLDER_SCREENSHOTS.map((shot, i) => (
          <div class="mock-gallery-item" key={i}>
            <div class="mock-gallery-thumb">
              <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M15 8h.01" /><path d="M3 6a3 3 0 0 1 3-3h12a3 3 0 0 1 3 3v12a3 3 0 0 1-3 3H6a3 3 0 0 1-3-3V6z" /><path d="M3 16l5-5c.928-.893 2.072-.893 3 0l5 5" /><path d="M14 14l1-1c.928-.893 2.072-.893 3 0l3 3" /></svg>
            </div>
            <div class="mock-gallery-meta">
              <div class="mock-gallery-date">{shot.date}</div>
              <div class="mock-gallery-dims">{shot.dims}</div>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}
