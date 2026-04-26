import type { JSX } from 'preact';
import type { EnrichedInstance, Version } from '../../types';

export function DetailsPane({ inst, version }: {
  inst: EnrichedInstance;
  version: Version | null;
}): JSX.Element {
  const isVanilla = !version?.inherits_from;
  const overrides: string[] = [];
  if (inst.java_path) overrides.push('Java path');
  if (inst.jvm_preset) overrides.push('JVM preset');
  if (inst.extra_jvm_args) overrides.push('Extra args');

  return (
    <div class="details-pane">
      <div class="details-grid">
        <div class="details-stat">
          <div class="details-stat-value">{inst.saves_count}</div>
          <div class="details-stat-label">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12a9 9 0 1 0 18 0a9 9 0 0 0-18 0" /><path d="M3.6 9h16.8" /><path d="M3.6 15h16.8" /><path d="M11.5 3a17 17 0 0 0 0 18" /><path d="M12.5 3a17 17 0 0 1 0 18" /></svg>
            Saves
          </div>
        </div>
        <div class="details-stat">
          <div class="details-stat-value">{inst.mods_count}</div>
          <div class="details-stat-label">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h3a1 1 0 0 0 1-1v-1a2 2 0 0 1 4 0v1a1 1 0 0 0 1 1h3a1 1 0 0 1 1 1v3a1 1 0 0 0 1 1h1a2 2 0 0 1 0 4h-1a1 1 0 0 0-1 1v3a1 1 0 0 1-1 1h-3a1 1 0 0 1-1-1v-1a2 2 0 0 0-4 0v1a1 1 0 0 1-1 1h-3a1 1 0 0 1-1-1v-3a1 1 0 0 1 1-1h1a2 2 0 0 0 0-4h-1a1 1 0 0 1-1-1v-3a1 1 0 0 1 1-1" /></svg>
            Mods
          </div>
        </div>
        <div class="details-stat">
          <div class="details-stat-value">{inst.resource_count}</div>
          <div class="details-stat-label">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 21a9 9 0 0 1 0-18c4.97 0 9 3.582 9 8c0 1.06-.474 2.078-1.318 2.828a4.007 4.007 0 0 1-2.682 1.172h-2.5a2 2 0 0 0-1 3.75a1.3 1.3 0 0 1-1.5 1.25" /><path d="M8.5 10.5m-1 0a1 1 0 1 0 2 0a1 1 0 1 0-2 0" /><path d="M12.5 7.5m-1 0a1 1 0 1 0 2 0a1 1 0 1 0-2 0" /><path d="M16.5 10.5m-1 0a1 1 0 1 0 2 0a1 1 0 1 0-2 0" /></svg>
            Resources
          </div>
        </div>
        <div class="details-stat">
          <div class="details-stat-value">{inst.shader_count}</div>
          <div class="details-stat-label">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12h1m8-9v1m8 8h1m-15.4-6.4l.7.7m12.1-.7l-.7.7" /><path d="M9 16a5 5 0 1 1 6 0a3.5 3.5 0 0 0-1 3a2 2 0 0 1-4 0a3.5 3.5 0 0 0-1-3" /><path d="M9.7 17h4.6" /></svg>
            Shaders
          </div>
        </div>
      </div>

      <div class="details-info-rows">
        <div class="details-info-row">
          <span class="details-info-label">Type</span>
          <span class="details-info-value">{isVanilla ? 'Vanilla' : 'Modded'}</span>
        </div>
        {version && (
          <div class="details-info-row">
            <span class="details-info-label">Status</span>
            <span class={`details-info-value${version.launchable ? ' accent' : ''}`}>
              {version.launchable ? 'Ready to launch' : version.status_detail || 'Needs install'}
            </span>
          </div>
        )}
        {overrides.length > 0 && (
          <div class="details-info-row">
            <span class="details-info-label">Overrides</span>
            <span class="details-info-value">{overrides.join(', ')}</span>
          </div>
        )}
        <div class="details-info-row">
          <span class="details-info-label">Instance ID</span>
          <span class="details-info-value details-info-mono">{inst.id}</span>
        </div>
      </div>
    </div>
  );
}
