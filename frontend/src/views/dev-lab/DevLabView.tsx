import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import './dev-lab.css';
import { ensureFlags, flagEnabled, refreshFlags, setFlagOverride } from '../../flags';
import { Sound } from '../../sound';
import { PRESET_HUES } from '../../state';
import {
  bootstrapState,
  config,
  featureFlags,
  featureFlagsLoadState,
  launchState,
  runningSessions,
  systemInfo,
  updateInfo,
} from '../../store';
import { activeDownload, downloadQueue } from '../../machines/downloads';
import { applyTheme, resetThemeToDefault } from '../../theme';
import { toast } from '../../toast';
import type { FeatureFlagViewModel } from '../../types-flags';
import type { ToastKind } from '../../types-ui';
import { Button, Card, Pill, Toggle } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { route } from '../../ui-state';

type LabTab = 'flags' | 'inspector' | 'playground';
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
    <div class="cp-dev-flag-row">
      <div class="cp-dev-flag-main">
        <div class="cp-dev-flag-head">
          <span class="cp-dev-flag-title">{flag.title}</span>
          <code class="cp-dev-flag-key">{flag.key}</code>
          <span class="cp-dev-flag-meta">
            <Pill tone={flag.stage === 'beta' ? 'info' : 'warn'}>{flag.stage}</Pill>
            {flag.dev_only && <Pill>dev only</Pill>}
            {custom && <Pill tone="accent">custom</Pill>}
          </span>
        </div>
        <div class="cp-dev-flag-description">{flag.description}</div>
      </div>
      <div class="cp-dev-flag-controls">
        {custom && (
          <Button variant="ghost" size="sm" icon="refresh" onClick={() => void setFlagOverride(flag.key, null)}>
            Reset
          </Button>
        )}
        <span class="cp-dev-flag-state" data-on={flag.enabled}>
          <span>{flag.enabled ? 'Enabled' : 'Disabled'}</span>
          <Toggle on={flag.enabled} onChange={() => void setFlagOverride(flag.key, !flag.enabled)} />
        </span>
      </div>
    </div>
  );
}

function FlagsPanel({ onRetry }: { onRetry: () => void }): JSX.Element {
  const flags = featureFlags.value;
  const loadState = featureFlagsLoadState.value;

  if (!flags) {
    const failed = loadState.status === 'error';
    const error = loadState.error || 'Unknown error';
    return (
      <div class="cp-dev-loading" data-error={failed} role={failed ? 'alert' : 'status'}>
        <span>{failed ? `Could not load feature flags: ${error}` : 'Feature flags are still loading.'}</span>
        {failed && (
          <Button variant="secondary" size="sm" icon="refresh" onClick={onRetry}>
            Retry
          </Button>
        )}
      </div>
    );
  }

  if (flags.length === 0) {
    return (
      <div class="cp-dev-empty" role="status">
        No feature flags are available.
      </div>
    );
  }

  return (
    <div class="cp-dev-sheet">
      <div class="cp-dev-flag-list">
        {flags.map((flag) => (
          <FlagRow key={flag.key} flag={flag} />
        ))}
      </div>
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
    { key: 'runningSessions', title: 'runningSessions', value: runningSessions.value },
    { key: 'updateInfo', title: 'updateInfo', value: updateInfo.value },
    { key: 'route', title: 'route', value: route.value, open: true },
  ];

  return (
    <div class="cp-dev-sheet cp-dev-inspector">
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
    <Card class="cp-dev-play-card">
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
    </Card>
  );
}

function SoundPlayground(): JSX.Element {
  return (
    <Card class="cp-dev-play-card">
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
    </Card>
  );
}

function ThemePlayground(): JSX.Element {
  return (
    <Card class="cp-dev-play-card">
      <div class="cp-dev-play-head">
        <div class="cp-dev-play-title">Theme</div>
        <div class="cp-dev-play-hint">Apply a preset hue to the live theme.</div>
      </div>
      <div class="cp-dev-play-actions">
        {Object.entries(PRESET_HUES).map(([theme, hue]) => (
          <Button key={theme} variant="secondary" size="sm" onClick={() => applyTheme(theme, hue)}>
            <span class="cp-dev-theme-button">
              <span class="cp-dev-theme-swatch" style={{ ['--cp-dev-hue' as any]: hue }} />
              {titleCase(theme)}
            </span>
          </Button>
        ))}
        <Button variant="ghost" size="sm" icon="refresh" onClick={() => resetThemeToDefault()}>
          Reset
        </Button>
      </div>
    </Card>
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

export function DevLabView(): JSX.Element {
  const [tab, setTab] = useState<LabTab>('flags');
  const inspectorAvailable = flagEnabled('dev.state-inspector');
  const activeTab = tab === 'inspector' && !inspectorAvailable ? 'flags' : tab;

  const loadFlags = (force = false): void => {
    const request = force ? refreshFlags() : ensureFlags();
    void request.catch(() => undefined);
  };

  useEffect(() => {
    loadFlags();
  }, []);

  useEffect(() => {
    if (tab === 'inspector' && !inspectorAvailable) setTab('flags');
  }, [tab, inspectorAvailable]);

  return (
    <div class="cp-view-page cp-dev-lab">
      <div class="cp-page-header">
        <div>
          <h1>Dev lab</h1>
          <div class="cp-page-sub">Developer workbench: feature flags, live state inspector, and UI playgrounds.</div>
        </div>
      </div>

      <div class="cp-dev-tabs">
        <button type="button" data-active={activeTab === 'flags'} onClick={() => setTab('flags')}>
          <Icon name="sliders" size={15} stroke={1.8} />
          Flags
        </button>
        {inspectorAvailable && (
          <button type="button" data-active={activeTab === 'inspector'} onClick={() => setTab('inspector')}>
            <Icon name="activity" size={15} stroke={1.8} />
            Inspector
          </button>
        )}
        <button type="button" data-active={activeTab === 'playground'} onClick={() => setTab('playground')}>
          <Icon name="cube" size={15} stroke={1.8} />
          Playground
        </button>
      </div>

      <div class="cp-dev-tab-panel">
        {activeTab === 'flags' && <FlagsPanel onRetry={() => loadFlags(true)} />}
        {activeTab === 'inspector' && inspectorAvailable && <InspectorPanel />}
        {activeTab === 'playground' && <PlaygroundPanel />}
      </div>
    </div>
  );
}
