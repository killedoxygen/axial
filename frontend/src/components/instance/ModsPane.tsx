import type { JSX } from 'preact';

const PLACEHOLDER_MODS = [
  { name: 'Sodium', version: '0.5.8', desc: 'Rendering engine replacement', enabled: true },
  { name: 'Lithium', version: '0.12.1', desc: 'General purpose optimization', enabled: true },
  { name: 'Iris Shaders', version: '1.7.0', desc: 'Shader pack loader', enabled: true },
  { name: 'Mod Menu', version: '9.0.0', desc: 'In game mod configuration', enabled: false },
  { name: 'Fabric API', version: '0.92.1', desc: 'Core library for Fabric mods', enabled: true },
];

export function ModsPane({ count, isVanilla }: { count: number; isVanilla: boolean }): JSX.Element {
  if (isVanilla) {
    return (
      <div class="mock-pane">
        <div class="mock-empty">
          <div class="mock-empty-icon">
            <svg width="32" height="32" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
              <path d="M4 7h3a1 1 0 0 0 1-1v-1a2 2 0 0 1 4 0v1a1 1 0 0 0 1 1h3a1 1 0 0 1 1 1v3a1 1 0 0 0 1 1h1a2 2 0 0 1 0 4h-1a1 1 0 0 0-1 1v3a1 1 0 0 1-1 1h-3a1 1 0 0 1-1-1v-1a2 2 0 0 0-4 0v1a1 1 0 0 1-1 1h-3a1 1 0 0 1-1-1v-3a1 1 0 0 1 1-1h1a2 2 0 0 0 0-4h-1a1 1 0 0 1-1-1v-3a1 1 0 0 1 1-1" />
            </svg>
          </div>
          <div class="mock-empty-title">No mod loader</div>
          <div class="mock-empty-text">This instance runs vanilla Minecraft. Install a mod loader like Fabric or Forge to start using mods.</div>
        </div>
      </div>
    );
  }

  return (
    <div class="mock-pane">
      <div class="mock-header">
        <div class="mock-header-title">
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h3a1 1 0 0 0 1-1v-1a2 2 0 0 1 4 0v1a1 1 0 0 0 1 1h3a1 1 0 0 1 1 1v3a1 1 0 0 0 1 1h1a2 2 0 0 1 0 4h-1a1 1 0 0 0-1 1v3a1 1 0 0 1-1 1h-3a1 1 0 0 1-1-1v-1a2 2 0 0 0-4 0v1a1 1 0 0 1-1 1h-3a1 1 0 0 1-1-1v-3a1 1 0 0 1 1-1h1a2 2 0 0 0 0-4h-1a1 1 0 0 1-1-1v-3a1 1 0 0 1 1-1" /></svg>
          {count} mod{count === 1 ? '' : 's'}
        </div>
        <div class="mock-header-right">
          <div class="mock-btn-sm">Add mod</div>
          <div class="mock-badge">Coming soon</div>
        </div>
      </div>
      <div class="mock-list">
        {PLACEHOLDER_MODS.map((mod) => (
          <div class={`mock-list-item${!mod.enabled ? ' mock-disabled' : ''}`} key={mod.name}>
            <div class="mock-list-icon">
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h3a1 1 0 0 0 1-1v-1a2 2 0 0 1 4 0v1a1 1 0 0 0 1 1h3a1 1 0 0 1 1 1v3a1 1 0 0 0 1 1h1a2 2 0 0 1 0 4h-1a1 1 0 0 0-1 1v3a1 1 0 0 1-1 1h-3a1 1 0 0 1-1-1v-1a2 2 0 0 0-4 0v1a1 1 0 0 1-1 1h-3a1 1 0 0 1-1-1v-3a1 1 0 0 1 1-1h1a2 2 0 0 0 0-4h-1a1 1 0 0 1-1-1v-3a1 1 0 0 1 1-1" /></svg>
            </div>
            <div class="mock-list-info">
              <div class="mock-list-name">{mod.name} <span class="mock-list-version">{mod.version}</span></div>
              <div class="mock-list-meta">{mod.desc}</div>
            </div>
            <div class="mock-list-toggle">
              <div class={`mock-toggle${mod.enabled ? ' on' : ''}`}>
                <div class="mock-toggle-knob" />
              </div>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}
