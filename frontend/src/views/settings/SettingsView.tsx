import type { JSX, ComponentChildren } from 'preact';
import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { Button, Card, Input, Pill } from '../../ui/Atoms';
import { SelectField } from '../../ui/Select';
import { Icon } from '../../ui/Icons';
import { Slider } from '../../ui/Slider';
import { AccentField, AccentModeToggle } from './AccentEditor';
import { local, saveLocalState, STORAGE_KEY } from '../../state';
import { Sound } from '../../sound';
import { Music, musicStateVersion } from '../../music';
import {
  config,
  systemInfo,
  devMode,
  appVersion,
  updateCheckState,
  updateInfo,
} from '../../store';
import { navigate, ROUTE_STORAGE_KEY } from '../../ui-state';
import { api } from '../../api';
import { toast } from '../../toast';
import { hasNativeDesktopRuntime, openExternalURL } from '../../native';
import { clampPlayerNameInput } from '../../player-name';
import { errMessage, fmtMem, getMemoryRecommendation, validateUsername } from '../../utils';
import {
  checkForUpdates,
  dismissAvailableUpdate,
  formatUpdateCheckTime,
  hasVisibleUpdate,
  openUpdateAction,
  openUpdateNotes,
  restartDesktopApp,
} from '../../updater';
import type {
  GuardianMode,
  PerformanceMode,
  PerformanceRulesStatus,
} from '../../types';

type SectionId = 'appearance' | 'gameplay' | 'performance' | 'audio' | 'shortcuts' | 'advanced' | 'about';

const SECTIONS: Array<{ id: SectionId; label: string; icon: string }> = [
  { id: 'appearance', label: 'Appearance', icon: 'palette' },
  { id: 'gameplay',   label: 'Gameplay',   icon: 'cube' },
  { id: 'performance', label: 'Performance', icon: 'shield-check' },
  { id: 'audio',      label: 'Audio',      icon: 'headphones' },
  { id: 'shortcuts',  label: 'Shortcuts',  icon: 'keyboard' },
  { id: 'advanced',   label: 'Advanced',   icon: 'terminal' },
  { id: 'about',      label: 'About',      icon: 'info' },
];

function SettingsCard({
  title, desc, control, stack, children,
}: {
  title: string;
  desc?: string;
  control?: ComponentChildren;
  stack?: boolean;
  children?: ComponentChildren;
}): JSX.Element {
  return (
    <Card class={`cp-settings-card${stack ? ' cp-settings-card--stack' : ''}`}>
      <div>
        <div class="cp-settings-card-title">{title}</div>
        {desc && <div class="cp-settings-card-desc">{desc}</div>}
        {stack && children}
      </div>
      {(control || (!stack && children)) && (
        <div class="cp-settings-card-control">{control || children}</div>
      )}
    </Card>
  );
}

type ModeOption<T extends string> = {
  value: T;
  label: string;
  note: string;
};

function ModeChoice<T extends string>({
  label,
  value,
  options,
  disabled,
  onChange,
}: {
  label: string;
  value: T;
  options: Array<ModeOption<T>>;
  disabled?: boolean;
  onChange: (value: T) => void;
}): JSX.Element {
  const selected = options.find((option) => option.value === value) ?? options[0];
  const selectId = `cp-settings-mode-${label.toLowerCase().replace(/[^a-z0-9]+/g, '-')}`;

  return (
    <div class="cp-settings-mode-choice">
      <label class="cp-settings-mode-choice-label" htmlFor={selectId}>{label}</label>
      <div class="cp-settings-mode-field">
        <SelectField<T>
          value={value}
          disabled={disabled}
          ariaLabel={label}
          onChange={onChange}
          options={options.map((option) => ({ value: option.value, label: option.label }))}
        />
        <div id={`${selectId}-note`} class="cp-settings-mode-note">
          {selected?.note ?? ''}
        </div>
      </div>
    </div>
  );
}

function performanceModeFrom(value: string | undefined): PerformanceMode {
  if (value === 'vanilla' || value === 'custom') return value;
  return 'managed';
}

function guardianModeFrom(value: string | undefined): GuardianMode {
  return value === 'custom' ? 'custom' : 'managed';
}

function Toggle({ on, onChange }: { on: boolean; onChange: () => void }): JSX.Element {
  return (
    <button
      type="button"
      class="cp-toggle"
      data-on={on}
      role="switch"
      aria-checked={on}
      onClick={onChange}
    />
  );
}


function AppearanceSection(): JSX.Element {
  return (
    <>
      <SettingsCard
        title="Mode"
        desc="Light or dark canvas. Accent colors re-derive automatically so contrast stays safe."
        control={<AccentModeToggle />}
      />
      <SettingsCard
        title="Accent"
        desc="Drag inside the field to pick any hue and chroma, or tap a preset. Every tint, ring, and on-accent contrast is derived from this single point."
        stack
      >
        <AccentField />
      </SettingsCard>
    </>
  );
}


function GameplaySection(): JSX.Element {
  const cfg = config.value;
  const sys = systemInfo.value;
  const savedUsername = cfg?.username || 'Player';
  const savedMemGB = (cfg?.max_memory_mb ?? 4096) / 1024;
  const [username, setUsername] = useState(cfg?.username || 'Player');
  const [memGB, setMemGB] = useState<number>(savedMemGB);
  const lastSaveRequest = useRef(0);
  const totalGB = sys?.total_memory_mb ? Math.floor(sys.total_memory_mb / 1024) : 16;
  const maxGB = Math.max(1, totalGB);
  const rec = getMemoryRecommendation(totalGB);
  const recHigh = Math.min(maxGB, rec.rec + 2);
  const recLow = Math.min(Math.max(2, rec.rec - 2), recHigh);
  const recZone: [number, number] = [recLow, recHigh];
  const memoryTicks = [1, Math.round(maxGB / 4), Math.round(maxGB / 2), Math.round(maxGB * 0.75), maxGB]
    .filter((value, index, values) => value >= 1 && value <= maxGB && values.indexOf(value) === index);

  useEffect(() => {
    setUsername(savedUsername);
    setMemGB(savedMemGB);
  }, [savedMemGB, savedUsername]);

  const recText = useMemo(() => {
    if (memGB < 2) return 'Low, may stutter';
    if (memGB > totalGB * 0.75) return 'Leave room for the OS';
    return rec.text;
  }, [memGB, totalGB, rec.text]);

  const nameError = validateUsername(username);
  const nameValid = nameError === null;
  const showNameError = username.length > 0 && !nameValid;
  const dirty = username !== savedUsername || memGB !== savedMemGB;

  const save = async (nextMemoryGB = memGB): Promise<void> => {
    const nextDirty = username !== savedUsername || nextMemoryGB !== savedMemGB;
    if (!nextDirty || !nameValid) return;
    const requestId = lastSaveRequest.current + 1;
    lastSaveRequest.current = requestId;
    try {
      const res: any = await api('PUT', '/config', {
        username: username.trim(),
        max_memory_mb: Math.round(nextMemoryGB * 1024),
      });
      if (res.error) throw new Error(res.error);
      if (requestId !== lastSaveRequest.current) return;
      config.value = res;
      toast('Saved');
    } catch (err) {
      if (requestId !== lastSaveRequest.current) return;
      toast(`Could not save settings: ${errMessage(err)}`);
    }
  };

  return (
    <>
      <SettingsCard
        title="Player name"
        desc="Shown to Minecraft at launch. Letters, numbers, or underscores (3–16)."
        stack
      >
        <div class="cp-settings-name">
          <Input
            value={username}
            onChange={(v) => setUsername(clampPlayerNameInput(v))}
            placeholder="Player"
            style={{ width: 240 }}
          />
          {dirty && <Button size="sm" onClick={() => { void save(); }} disabled={!nameValid} sound="affirm">Save</Button>}
          {showNameError && <span class="cp-settings-name-err">{nameError}</span>}
        </div>
      </SettingsCard>
      <SettingsCard
        title="Memory"
        desc={`Maximum RAM given to the JVM when launching. ${recText} (system has ${totalGB} GB).`}
        stack
      >
        <div style={{ marginTop: 14 }}>
          <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: 12, marginBottom: 6 }}>
            <span style={{ color: 'var(--text-mute)' }}>Allocation</span>
            <span style={{ color: 'var(--text)', fontWeight: 700 }}>{fmtMem(memGB)}</span>
          </div>
          <Slider
            value={memGB}
            min={1} max={maxGB} step={0.5}
            recommended={recZone}
            ticks={memoryTicks}
            sound="memory"
            onChange={setMemGB}
            onCommit={(next) => { void save(next); }}
            ariaLabel="Max memory in gigabytes"
          />
        </div>
      </SettingsCard>
    </>
  );
}


const PERFORMANCE_OPTIONS: Array<ModeOption<PerformanceMode>> = [
  { value: 'managed', label: 'Managed', note: 'Recommended defaults' },
  { value: 'vanilla', label: 'Vanilla', note: 'No add-ons' },
  { value: 'custom', label: 'Custom', note: 'Manual tuning' },
];

const GUARDIAN_OPTIONS: Array<ModeOption<GuardianMode>> = [
  { value: 'managed', label: 'Managed', note: 'Warns and protects' },
  { value: 'custom', label: 'Custom', note: 'Preserves; blocks fatal' },
];

type RulesStatusState =
  | { status: 'loading'; data: null; error?: undefined }
  | { status: 'ready'; data: PerformanceRulesStatus; error?: undefined }
  | { status: 'error'; data: null; error: string };

function healthStateLabel(value: string): string {
  return value
    .split('_')
    .map((part) => part[0]?.toUpperCase() + part.slice(1))
    .join(' ');
}

function ownershipLabel(value: string): string {
  if (value === 'composition_managed') return 'Croopor-managed';
  if (value === 'user_managed') return 'User-managed';
  return healthStateLabel(value);
}

function formatRuleDate(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return new Intl.DateTimeFormat(undefined, { year: 'numeric', month: 'short', day: 'numeric' }).format(date);
}

function emergencyDisableSummary(status: PerformanceRulesStatus): string {
  const count = status.emergency_disable_count ?? status.emergency_disables?.length ?? 0;
  if (count === 0) return 'None active';
  const firstReason = status.emergency_disables?.[0]?.reason?.trim();
  const prefix = `${count} active`;
  return firstReason ? `${prefix}: ${firstReason}` : prefix;
}

function rulesCacheSummary(status: PerformanceRulesStatus): string {
  const cache = status.rules_cache;
  if (cache?.state === 'invalid') return 'Invalid local cache';
  if (!cache?.recorded) return 'Unavailable';
  return 'Recorded locally';
}

function PerformanceRulesStatusBlock({
  state,
  standalone = false,
}: {
  state: RulesStatusState;
  standalone?: boolean;
}): JSX.Element {
  const className = `cp-settings-rule-status${standalone ? ' cp-settings-rule-status--standalone' : ''}`;

  if (state.status === 'loading') {
    return (
      <div class={className} aria-live="polite">
        <div class="cp-settings-rule-status-copy">
          <strong>Loading rule status</strong>
          <span>Checking the active performance rule source.</span>
        </div>
      </div>
    );
  }

  if (state.status === 'error') {
    return (
      <div class={`${className} cp-settings-rule-status--warn`} aria-live="polite">
        <div class="cp-settings-rule-status-copy">
          <strong>Rule status unavailable</strong>
          <span>{state.error || 'Performance controls still use saved settings.'}</span>
        </div>
      </div>
    );
  }

  const status = state.data;
  const source = status.rule_source === 'built_in' ? 'Built-in rules' : healthStateLabel(status.rule_source);
  const channel = status.rule_channel === 'bundled' ? 'bundled manifest' : healthStateLabel(status.rule_channel);
  const refresh = status.remote_refresh
    ? status.last_refresh_at
      ? `Last refreshed ${formatRuleDate(status.last_refresh_at)}`
      : 'Remote refresh configured, not refreshed yet'
    : 'Remote refresh off';

  return (
    <div class={className} aria-live="polite">
      <div class="cp-settings-rule-status-head">
        <div class="cp-settings-rule-status-copy">
          <strong>{source} active</strong>
          <span>
            {status.validation === 'valid'
              ? 'Managed performance defaults are ready.'
              : 'Managed performance rules need attention.'}
          </span>
        </div>
        <Pill tone={status.validation === 'valid' ? 'ok' : 'err'} icon={status.validation === 'valid' ? 'check' : 'alert'}>
          {status.validation === 'valid' ? 'Valid' : 'Invalid'}
        </Pill>
      </div>
      <div class="cp-settings-rule-status-meta">
        <span>Source</span>
        <strong>{channel}</strong>
        <span>Refresh</span>
        <strong>{refresh}</strong>
        <span>Compositions</span>
        <strong>{status.composition_count}</strong>
      </div>
      <details class="cp-settings-rule-details">
        <summary>
          Rule details{status.warnings.length > 0 ? `, ${status.warnings.length} warning${status.warnings.length === 1 ? '' : 's'}` : ''}
        </summary>
        {status.warnings.length > 0 && (
          <div class="cp-settings-rule-status-warnings">
            {status.warnings.map((warning) => <span key={warning}>{warning}</span>)}
          </div>
        )}
        <div class="cp-settings-rule-status-grid">
          <span>Schema</span>
          <strong>v{status.schema_version}</strong>
          <span>Generated</span>
          <strong>{formatRuleDate(status.generated_at)}</strong>
          <span>Rules cache</span>
          <strong>{rulesCacheSummary(status)}</strong>
          <span>Emergency disables</span>
          <strong>{emergencyDisableSummary(status)}</strong>
          <span>Bundle health</span>
          <strong>{status.health_states.map(healthStateLabel).join(', ')}</strong>
          <span>Ownership</span>
          <strong>{status.ownership_classes.map(ownershipLabel).join(', ')}</strong>
        </div>
      </details>
    </div>
  );
}

function PerformanceSection(): JSX.Element {
  const cfg = config.value;
  const savedPerformance = performanceModeFrom(cfg?.performance_mode);
  const savedGuardian = guardianModeFrom(cfg?.guardian_mode);
  const [performanceMode, setPerformanceMode] = useState<PerformanceMode>(savedPerformance);
  const [guardianMode, setGuardianMode] = useState<GuardianMode>(savedGuardian);
  const [rulesStatus, setRulesStatus] = useState<RulesStatusState>({ status: 'loading', data: null });
  const [saving, setSaving] = useState<'performance' | 'guardian' | null>(null);
  const requestRef = useRef(0);

  useEffect(() => {
    setPerformanceMode(savedPerformance);
    setGuardianMode(savedGuardian);
  }, [savedPerformance, savedGuardian]);

  useEffect(() => {
    let alive = true;
    setRulesStatus({ status: 'loading', data: null });
    api('GET', '/performance/status')
      .then((res) => {
        if (!alive) return;
        if (res?.error) throw new Error(res.error);
        setRulesStatus({ status: 'ready', data: res as PerformanceRulesStatus });
      })
      .catch((err) => {
        if (!alive) return;
        setRulesStatus({ status: 'error', data: null, error: errMessage(err) });
      });
    return () => { alive = false; };
  }, []);

  const savePatch = async (
    key: 'performance_mode' | 'guardian_mode',
    value: PerformanceMode | GuardianMode,
  ): Promise<void> => {
    const savingKey = key === 'performance_mode' ? 'performance' : 'guardian';
    const requestId = requestRef.current + 1;
    requestRef.current = requestId;
    setSaving(savingKey);
    try {
      const res: any = await api('PUT', '/config', { [key]: value });
      if (res?.error) throw new Error(res.error);
      if (requestId !== requestRef.current) return;
      config.value = res;
      toast('Saved');
    } catch (err) {
      if (requestId !== requestRef.current) return;
      setPerformanceMode(savedPerformance);
      setGuardianMode(savedGuardian);
      const settingLabel = key === 'performance_mode' ? 'performance settings' : 'Guardian settings';
      toast(`Could not save ${settingLabel}: ${errMessage(err)}`, 'error');
    } finally {
      if (requestId === requestRef.current) setSaving(null);
    }
  };

  const changePerformance = (next: PerformanceMode): void => {
    if (next === performanceMode) return;
    setPerformanceMode(next);
    void savePatch('performance_mode', next);
  };

  const changeGuardian = (next: GuardianMode): void => {
    if (next === guardianMode) return;
    setGuardianMode(next);
    void savePatch('guardian_mode', next);
  };

  return (
    <>
      <SettingsCard
        title="Performance"
        desc="Launch behavior and managed rule readiness."
        stack
      >
        <div class="cp-settings-mode-grid">
          <ModeChoice
            label="Performance mode"
            value={performanceMode}
            options={PERFORMANCE_OPTIONS}
            disabled={saving !== null}
            onChange={changePerformance}
          />
          <ModeChoice
            label="Guardian mode"
            value={guardianMode}
            options={GUARDIAN_OPTIONS}
            disabled={saving !== null}
            onChange={changeGuardian}
          />
        </div>
        <PerformanceRulesStatusBlock state={rulesStatus} />
      </SettingsCard>
    </>
  );
}

type PerformanceLabCardComponent = typeof import('./PerformanceLabCard')['PerformanceLabCard'];

function PerformanceLabSlot(): JSX.Element | null {
  const isDev = devMode.value;
  const [Lab, setLab] = useState<PerformanceLabCardComponent | null>(null);

  useEffect(() => {
    if (!isDev) {
      setLab(null);
      return;
    }

    let alive = true;
    void import('./PerformanceLabCard').then((module) => {
      if (alive) setLab(() => module.PerformanceLabCard);
    });
    return () => { alive = false; };
  }, [isDev]);

  if (!isDev || !Lab) return null;
  return <Lab />;
}


function AudioSection(): JSX.Element {
  musicStateVersion.value;
  const [soundsOn, setSoundsOn] = useState<boolean>(local.sounds);
  const [musicOn, setMusicOn] = useState<boolean>(Music.enabled);
  const [volume, setVolume] = useState<number>(Music.volume);

  useEffect(() => { setMusicOn(Music.enabled); setVolume(Music.volume); }, [musicStateVersion.value]);

  const toggleSounds = (): void => {
    const next = !soundsOn;
    setSoundsOn(next);
    local.sounds = next;
    Sound.enabled = next;
    saveLocalState();
    if (next) Sound.ui('affirm');
  };

  const toggleMusic = (): void => {
    Music.toggle();
    setMusicOn(Music.enabled);
  };

  return (
    <>
      <SettingsCard
        title="UI sounds"
        desc="Soft audio feedback for buttons, sliders, and theme changes."
        control={<Toggle on={soundsOn} onChange={toggleSounds} />}
      />
      <SettingsCard
        title="Background music"
        desc="Ambient track while you're in the launcher. Pauses automatically during gameplay."
        control={<Toggle on={musicOn} onChange={toggleMusic} />}
      />
      {musicOn && (
        <SettingsCard title="Music volume" desc="Set the ambient level without muting." stack>
          <div style={{ marginTop: 14 }}>
            <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: 12, marginBottom: 6 }}>
              <span style={{ color: 'var(--text-mute)' }}>Volume</span>
              <span style={{ color: 'var(--text)', fontWeight: 700 }}>{volume}%</span>
            </div>
            <Slider
              value={volume} min={0} max={100} step={1}
              sound="volume"
              onChange={(v) => {
                setVolume(v);
                Music.setVolume(v);
              }}
              ariaLabel="Music volume"
            />
          </div>
        </SettingsCard>
      )}
    </>
  );
}


function ShortcutsSection(): JSX.Element {
  const rows: Array<[string, string]> = [
    ['Open settings', 'Ctrl + ,'],
    ['Focus search', 'Ctrl + F'],
    ['New instance', 'Ctrl + N'],
    ['Launch selected', 'Ctrl + Enter'],
    ['Close dialogs', 'Esc'],
  ];
  return (
    <SettingsCard title="Keyboard shortcuts" desc="Global shortcuts available in the launcher." stack>
      <div style={{ marginTop: 14, display: 'flex', flexDirection: 'column', gap: 2 }}>
        {rows.map(([label, combo]) => (
          <div key={label} style={{
            display: 'flex', justifyContent: 'space-between', alignItems: 'center',
            padding: '8px 4px', borderBottom: '1px dashed var(--line)',
          }}>
            <span style={{ fontSize: 13, color: 'var(--text)' }}>{label}</span>
            <kbd class="cp-kbd">{combo}</kbd>
          </div>
        ))}
      </div>
    </SettingsCard>
  );
}


function AdvancedSection(): JSX.Element {
  const cfg = config.value;
  const isDev = devMode.value;
  const savedTelemetry = cfg?.telemetry_enabled === true;
  const [telemetryEnabled, setTelemetryEnabled] = useState(savedTelemetry);
  const [savingTelemetry, setSavingTelemetry] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    setTelemetryEnabled(savedTelemetry);
  }, [savedTelemetry]);

  const toggleTelemetry = async (): Promise<void> => {
    if (savingTelemetry) return;
    const next = !telemetryEnabled;
    setTelemetryEnabled(next);
    setSavingTelemetry(true);
    try {
      const res: any = await api('PUT', '/config', { telemetry_enabled: next });
      if (res?.error) throw new Error(res.error);
      config.value = res;
      toast('Saved');
    } catch (err) {
      setTelemetryEnabled(savedTelemetry);
      toast(`Could not save diagnostics setting: ${errMessage(err)}`, 'error');
    } finally {
      setSavingTelemetry(false);
    }
  };

  const flush = async (): Promise<void> => {
    const { showConfirm } = await import('../../ui/Dialog');
    const ok = await showConfirm('Delete all Croopor-owned data and reset the launcher to first run?', { destructive: true, confirmText: 'Reset' });
    if (!ok) return;
    setBusy(true);
    try {
      await api('POST', '/dev/flush');
      for (const key of [STORAGE_KEY, ROUTE_STORAGE_KEY]) {
        localStorage.removeItem(key);
      }
      location.reload();
    } catch (err) {
      toast(`Failed: ${errMessage(err)}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <SettingsCard
        title="Optional diagnostics"
        desc="Stores diagnostics consent. Current builds do not upload telemetry or open a remote diagnostics channel."
        control={<Toggle on={telemetryEnabled} onChange={() => void toggleTelemetry()} />}
      />
      <SettingsCard
        title="Reload launcher"
        desc="Useful if the launcher gets out of sync with the backend."
        control={<Button variant="secondary" icon="refresh" onClick={() => location.reload()}>Reload</Button>}
      />
      {__CROOPOR_ENABLE_DEV_LAB__ && isDev && (
        <SettingsCard
          title="Dev lab"
          desc="Developer-only workbench for procedural art and internal experiments."
          control={<Button variant="secondary" icon="palette" onClick={() => navigate({ name: 'dev-lab' })}>Open lab</Button>}
        />
      )}
      {isDev && <PerformanceLabSlot />}
      {isDev && (
        <SettingsCard
          title="Flush all data"
          desc="Deletes every Croopor-managed file and restarts from first run. Existing libraries selected through 'Use existing' are preserved."
          control={<Button variant="danger" icon="trash" disabled={busy} onClick={flush}>{busy ? 'Flushing…' : 'Flush'}</Button>}
        />
      )}
    </>
  );
}


function displayReleaseVersion(version: string): string {
  return version.startsWith('v') || version.startsWith('V') ? version : `v${version}`;
}

async function openHomepage(): Promise<void> {
  try {
    await openExternalURL('https://github.com/mateoltd/croopor');
    toast('Opened homepage');
  } catch (err: unknown) {
    toast(`Failed to open homepage: ${errMessage(err)}`, 'error');
  }
}

function AboutSection(): JSX.Element {
  const info = updateInfo.value;
  const checkState = updateCheckState.value;
  const [, setDismissedAt] = useState(0);
  const checking = checkState === 'checking';
  const latestVersion = info?.latest_version || appVersion.value;
  const status = checking
    ? 'Checking for updates...'
    : info
      ? info.available
        ? `Latest release: ${displayReleaseVersion(latestVersion)}`
        : `Current release: ${displayReleaseVersion(info.current_version)}`
      : 'Updates have not been checked yet.';
  const visibleUpdate = hasVisibleUpdate();
  const checkedAt = info ? formatUpdateCheckTime(info.checked_at) : 'Not checked yet';

  const dismiss = (): void => {
    dismissAvailableUpdate();
    setDismissedAt(Date.now());
  };

  return (
    <SettingsCard title="Croopor" desc={`Version ${appVersion.value}. A focused Minecraft launcher.`} stack>
      <div style={{ marginTop: 12, display: 'flex', gap: 8, flexWrap: 'wrap' }}>
        <Button variant="secondary" icon="globe" onClick={() => void openHomepage()}>Homepage</Button>
        <Button variant="secondary" icon="refresh" disabled={checking} onClick={() => void checkForUpdates({ force: true })}>
          {checking ? 'Checking...' : 'Check'}
        </Button>
        {hasNativeDesktopRuntime() && (
          <Button variant="secondary" icon="refresh" onClick={() => void restartDesktopApp()}>Restart</Button>
        )}
      </div>
      <div style={{ marginTop: 12, color: 'var(--text)', fontSize: 13, fontWeight: 700 }}>{status}</div>
      <div style={{ marginTop: 4, color: 'var(--text-mute)', fontSize: 12 }}>Last checked: {checkedAt}</div>
      {checkState === 'error' && (
        <div style={{ marginTop: 8, color: 'var(--err)', fontSize: 12 }}>Could not check for updates.</div>
      )}
      {visibleUpdate && (
        <div style={{ marginTop: 12, display: 'flex', gap: 8, flexWrap: 'wrap' }}>
          <Button variant="primary" icon="globe" onClick={() => void openUpdateAction()}>
            {info?.action_label || 'Open release'}
          </Button>
          <Button variant="secondary" icon="tag" onClick={() => void openUpdateNotes()}>Notes</Button>
          <Button variant="secondary" icon="x" onClick={dismiss}>Dismiss</Button>
        </div>
      )}
    </SettingsCard>
  );
}

export function SettingsView(): JSX.Element {
  const [section, setSection] = useState<SectionId>('appearance');

  return (
    <div class="cp-settings">
      <aside class="cp-settings-rail">
        <h1>Settings</h1>
        <div class="cp-settings-rail-list">
          {SECTIONS.map(s => (
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
        {section === 'gameplay' && <GameplaySection />}
        {section === 'performance' && <PerformanceSection />}
        {section === 'audio' && <AudioSection />}
        {section === 'shortcuts' && <ShortcutsSection />}
        {section === 'advanced' && <AdvancedSection />}
        {section === 'about' && <AboutSection />}
      </div>
    </div>
  );
}
