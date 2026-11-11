import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import './dev-lab.css';
import { ensureFlags, flagEnabled, refreshFlags, setFlagOverride } from '../../flags';
import { Sound } from '../../sound';
import { PRESET_HUES } from '../../state';
import {
  bootstrapState,
  config,
  devMode,
  featureFlags,
  featureFlagsLoadState,
  launchState,
  launchSessions,
  systemInfo,
  updateInfo,
} from '../../store';
import { activeDownload, downloadQueue } from '../../machines/downloads';
import { applyTheme, resetThemeToDefault } from '../../theme';
import { toast } from '../../toast';
import type { FeatureFlagViewModel } from '../../types-flags';
import type { ToastKind } from '../../types-ui';
import { Button, Chip, Empty, Status, Tabs, Toggle, type TabItem } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { Primitives } from './Primitives';
import { OverrideChip } from '../../ui/SettingsSheet';
import { route } from '../../ui-state';

type LabTab = 'flags' | 'inspector' | 'playground' | 'primitives';
type SoundKind = Parameters<typeof Sound.ui>[0];

const TOAST_KINDS: ToastKind[] = ['success', 'info', 'error'];

const SOUND_KINDS: Array<{ kind: SoundKind; label: string; value?: number }> = [
  { kind: 'soft', label: 'Soft' },
  { kind: 'click', label: 'Click' },
  { kind: 'bright', label: 'Bright' },
  { kind: 'affirm', label: 'Affirm' },
  { kind: 'theme', label: 'Theme' },
  { kind: 'slider', label: 'Slider', value: 0.7 },
  { kind: 'launchPress', label: 'Launch press' },
  { kind: 'launchSuccess', label: 'Launch success' },
];

function formatJson(value: unknown): string {
  try {
    return JSON.stringify(value, null, 2) ?? 'undefined';
  } catch {
    return '[Unable to serialize value]';
  }
}

function titleCase(value: string): string {
  return value
    .split(/[-_ ]+/)
    .filter(Boolean)
    .map((part) => part.slice(0, 1).toUpperCase() + part.slice(1))
    .join(' ');
}

function FlagRow({ flag }: { flag: FeatureFlagViewModel }): JSX.Element {
  const custom = flag.source === 'override';
  return (
    <div class="cp-dev-flag">
      <Toggle on={flag.enabled} ariaLabel={flag.title} onChange={() => void setFlagOverride(flag.key, !flag.enabled)} />
      <div class="cp-dev-flag-main">
        <div class="cp-dev-flag-head">
          <span class="cp-dev-flag-title">{flag.title}</span>
          <code class="cp-dev-flag-key">{flag.key}</code>
          <Chip tone={flag.stage === 'beta' ? 'info' : 'warn'}>{flag.stage}</Chip>
          {flag.dev_only && <Chip>dev only</Chip>}
          {custom && <OverrideChip label="Custom" onReset={() => void setFlagOverride(flag.key, null)} />}
        </div>
        <div class="cp-dev-flag-description">{flag.description}</div>
      </div>
    </div>
  );
}

function FlagsPanel({ onRetry }: { onRetry: () => void }): JSX.Element {
  const flags = featureFlags.value;
  const loadState = featureFlagsLoadState.value;

  if (!flags) {
    if (loadState.status !== 'error') return <Status busy>Loading feature flags</Status>;
    return (
      <Empty
        eyes="x"
        title="Could not load feature flags"
        actions={
          <Button variant="secondary" onClick={onRetry}>
            Retry
          </Button>
        }
      >
        {loadState.error || 'Unknown error'}
      </Empty>
    );
  }

  if (flags.length === 0) return <Empty title="No feature flags are available" />;

  return (
    <div class="cp-dev-flags">
      {flags.map((flag) => (
        <FlagRow key={flag.key} flag={flag} />
      ))}
    </div>
  );
}

function InspectorPanel(): JSX.Element {
  const items = [
    { key: 'bootstrapState', title: 'bootstrapState', value: bootstrapState.value, open: true },
    { key: 'config', title: 'config', value: config.value },
    { key: 'systemInfo', title: 'systemInfo', value: systemInfo.value },
    { key: 'activeDownload', title: 'activeDownload', value: activeDownload.value },
    { key: 'downloadQueue', title: 'downloadQueue', value: downloadQueue.value },
    { key: 'launchState', title: 'launchState', value: launchState.value },
    { key: 'launchSessions', title: 'launchSessions', value: launchSessions.value },
    { key: 'updateInfo', title: 'updateInfo', value: updateInfo.value },
    { key: 'route', title: 'route', value: route.value, open: true },
  ];

  return (
    <div class="cp-dev-list">
      {items.map((item) => (
        <details key={item.key} class="cp-dev-inspector-row" open={item.open}>
          <summary>
            <Icon name="chevron-right" size={14} stroke={2} />
            <span class="cp-dev-inspector-name">{item.title}</span>
          </summary>
          <div class="cp-dev-inspector-body">
            <pre>{formatJson(item.value)}</pre>
          </div>
        </details>
      ))}
    </div>
  );
}

function ToastPlayground(): JSX.Element {
  return (
    <div class="cp-dev-play-card">
      <div class="cp-dev-play-head">
        <div class="cp-dev-play-title">Toasts</div>
        <div class="cp-dev-play-hint">Fire a test toast in each tone.</div>
      </div>
      <div class="cp-dev-play-actions">
        {TOAST_KINDS.map((kind) => (
          <Button key={kind} variant="secondary" size="sm" onClick={() => toast('Test toast', kind)}>
            {titleCase(kind)}
          </Button>
        ))}
      </div>
    </div>
  );
}

function SoundPlayground(): JSX.Element {
  return (
    <div class="cp-dev-play-card">
      <div class="cp-dev-play-head">
        <div class="cp-dev-play-title">Sounds</div>
        <div class="cp-dev-play-hint">Audition the interface sound cues.</div>
      </div>
      <div class="cp-dev-play-actions">
        {SOUND_KINDS.map(({ kind, label, value }) => (
          <Button key={kind} variant="secondary" size="sm" sound={false} onClick={() => Sound.ui(kind, value)}>
            {label}
          </Button>
        ))}
      </div>
    </div>
  );
}

function ThemePlayground(): JSX.Element {
  return (
    <div class="cp-dev-play-card">
      <div class="cp-dev-play-head">
        <div class="cp-dev-play-title">Theme</div>
        <div class="cp-dev-play-hint">Apply a preset hue to the live theme.</div>
      </div>
      <div class="cp-dev-play-actions">
        {Object.entries(PRESET_HUES).map(([theme, hue]) => (
          <Button key={theme} variant="secondary" size="sm" onClick={() => applyTheme(theme, hue)}>
            <span class="cp-dev-theme-button">
              <span class="cp-dev-theme-swatch" style={{ ['--hue' as string]: hue }} />
              {titleCase(theme)}
            </span>
          </Button>
        ))}
        <Button variant="ghost" size="sm" onClick={() => resetThemeToDefault()}>
          Reset
        </Button>
      </div>
    </div>
  );
}

function PlaygroundPanel(): JSX.Element {
  return (
    <div class="cp-dev-playground-grid">
      <ToastPlayground />
      <SoundPlayground />
      <ThemePlayground />
    </div>
  );
}

export function DevLabView(): JSX.Element | null {
  const [tab, setTab] = useState<LabTab>('flags');
  const available = __AXIAL_ENABLE_DEV_LAB__ && devMode.value;
  const inspectorAvailable = flagEnabled('dev.state-inspector');
  const activeTab = tab === 'inspector' && !inspectorAvailable ? 'flags' : tab;

  const loadFlags = (force = false): void => {
    const request = force ? refreshFlags() : ensureFlags();
    void request.catch(() => undefined);
  };

  useEffect(() => {
    if (available) loadFlags();
  }, [available]);

  useEffect(() => {
    if (tab === 'inspector' && !inspectorAvailable) setTab('flags');
  }, [tab, inspectorAvailable]);

  if (!available) return null;

  const tabs: TabItem<LabTab>[] = [
    { id: 'flags', label: 'Flags', icon: 'sliders' },
    ...(inspectorAvailable ? [{ id: 'inspector' as const, label: 'Inspector', icon: 'activity' as const }] : []),
    { id: 'playground', label: 'Playground', icon: 'cube' },
    { id: 'primitives', label: 'Primitives', icon: 'palette' },
  ];

  return (
    <div class="cp-view-page">
      <h1 class="cp-h1">Dev lab</h1>
      <Tabs tabs={tabs} active={activeTab} label="Dev lab sections" onSelect={setTab} />
      <div role="tabpanel">
        {activeTab === 'flags' && <FlagsPanel onRetry={() => loadFlags(true)} />}
        {activeTab === 'inspector' && inspectorAvailable && <InspectorPanel />}
        {activeTab === 'playground' && <PlaygroundPanel />}
        {activeTab === 'primitives' && <Primitives />}
      </div>
    </div>
  );
}
