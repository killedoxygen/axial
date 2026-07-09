import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
import { Icon } from '../../ui/Icons';
import { AppearanceSection } from './AppearanceSection';
import { AudioSection } from './AudioSection';
import { LaunchingSection } from './LaunchingSection';
import { PerformanceSection } from './PerformanceSection';
import { ShortcutsSection } from './ShortcutsSection';
import { AdvancedSettingsSection } from './AdvancedSettingsSection';
import { AboutSettingsSection } from './AboutSettingsSection';

type SectionId = 'appearance' | 'audio' | 'launching' | 'performance' | 'shortcuts' | 'advanced' | 'about';

const SECTIONS: Array<{ id: SectionId; label: string; icon: string }> = [
  { id: 'appearance', label: 'Appearance', icon: 'palette' },
  { id: 'audio', label: 'Audio', icon: 'headphones' },
  { id: 'launching', label: 'Launching', icon: 'play' },
  { id: 'performance', label: 'Performance', icon: 'shield-check' },
  { id: 'shortcuts', label: 'Shortcuts', icon: 'keyboard' },
  { id: 'advanced', label: 'Advanced', icon: 'terminal' },
  { id: 'about', label: 'About', icon: 'info' },
];

export function SettingsView(): JSX.Element {
  const [section, setSection] = useState<SectionId>('appearance');

  return (
    <div class="cp-settings">
      <aside class="cp-settings-rail">
        <h1>Settings</h1>
        <div class="cp-settings-rail-list">
          {SECTIONS.map((s) => (
            <button
              key={s.id}
              class="cp-settings-rail-btn"
              data-active={section === s.id}
              onClick={() => setSection(s.id)}
            >
              <Icon name={s.icon} size={16} stroke={1.8} />
              {s.label}
            </button>
          ))}
        </div>
      </aside>
      <div class="cp-settings-body">
        {section === 'appearance' && <AppearanceSection />}
        {section === 'audio' && <AudioSection />}
        {section === 'launching' && <LaunchingSection />}
        {section === 'performance' && <PerformanceSection />}
        {section === 'shortcuts' && <ShortcutsSection />}
        {section === 'advanced' && <AdvancedSettingsSection />}
        {section === 'about' && <AboutSettingsSection />}
      </div>
    </div>
  );
}
