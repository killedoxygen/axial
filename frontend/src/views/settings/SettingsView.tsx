import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
import { Tabs, type TabItem } from '../../ui/Atoms';
import { resetViewScroll } from '../../ui-state';
import { AppearanceSection } from './AppearanceSection';
import { AudioSection } from './AudioSection';
import { LaunchingSection } from './LaunchingSection';
import { PerformanceSection } from './PerformanceSection';
import { ShortcutsSection } from './ShortcutsSection';
import { AdvancedSettingsSection } from './AdvancedSettingsSection';
import { AboutSettingsSection } from './AboutSettingsSection';

type SectionId = 'appearance' | 'audio' | 'launching' | 'performance' | 'shortcuts' | 'advanced' | 'about';

const SECTIONS: TabItem<SectionId>[] = [
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
      <Tabs
        tabs={SECTIONS}
        active={section}
        label="Settings sections"
        onSelect={(id) => {
          setSection(id);
          resetViewScroll();
        }}
      />
      <div class="cp-settings-page" role="tabpanel">
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
