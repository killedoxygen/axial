import type { JSX } from 'preact';
import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { Button, Input, Pill, Toggle } from '../../ui/Atoms';
import { Segmented } from '../../ui/Segmented';
import { Icon } from '../../ui/Icons';
import { Slider } from '../../ui/Slider';
import { AccentField, AccentModeToggle } from './AccentEditor';
import { local, saveLocalState } from '../../state';
import { Sound } from '../../sound';
import { Music, musicStateVersion } from '../../music';
import { config, systemInfo } from '../../store';
import { api } from '../../api';
import { toast } from '../../toast';
import { clampPlayerNameInput } from '../../player-name';
import { errMessage, fmtMem, getMemoryRecommendation, validateUsername } from '../../utils';
import type { GuardianMode } from '../../types-guardian';
import type { PerformanceMode, PerformanceRulesStatus } from '../../types-performance';
import { AboutSettingsSection } from './AboutSettingsSection';
import { AdvancedSettingsSection } from './AdvancedSettingsSection';
import { SettingsCard } from './settings-shared';

type SectionId = 'appearance' | 'gameplay' | 'performance' | 'audio' | 'shortcuts' | 'advanced' | 'about';

const SECTIONS: Array<{ id: SectionId; label: string; icon: string }> = [
  { id: 'appearance', label: 'Appearance', icon: 'palette' },
  { id: 'gameplay', label: 'Gameplay', icon: 'stack' },
  { id: 'performance', label: 'Performance', icon: 'shield-check' },
  { id: 'audio', label: 'Audio', icon: 'headphones' },
  { id: 'shortcuts', label: 'Shortcuts', icon: 'keyboard' },
  { id: 'advanced', label: 'Advanced', icon: 'terminal' },
  { id: 'about', label: 'About', icon: 'info' },
];

type ModeOption<T extends string> = {
  value: T;
  label: string;
  icon: string;
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

  return (
    <div class="cp-settings-mode-choice" data-disabled={disabled ? 'true' : 'false'}>
      <span class="cp-settings-mode-choice-label">{label}</span>
      <div class="cp-settings-mode-seg" aria-label={label}>
        <Segmented<T>
          value={value}
          onChange={(next) => {
            if (disabled) return;
            onChange(next);
          }}
          options={options.map((option) => ({ value: option.value, label: option.label, icon: option.icon }))}
        />
      </div>
      <p class="cp-settings-mode-note" aria-live="polite">
        {selected?.note ?? ''}
      </p>
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
  const savedDiscordRpc = cfg?.discord_rpc_enabled !== false;
  const [username, setUsername] = useState(cfg?.username || 'Player');
  const [memGB, setMemGB] = useState<number>(savedMemGB);
  const [discordRpcEnabled, setDiscordRpcEnabled] = useState(savedDiscordRpc);
  const [savingDiscordRpc, setSavingDiscordRpc] = useState(false);
  const lastSaveRequest = useRef(0);
  const totalGB = sys?.total_memory_mb ? Math.floor(sys.total_memory_mb / 1024) : 16;
  const maxGB = Math.max(1, totalGB);
  const rec = getMemoryRecommendation(totalGB);
  const recHigh = Math.min(maxGB, rec.rec + 2);
  const recLow = Math.min(Math.max(2, rec.rec - 2), recHigh);
  const recZone: [number, number] = [recLow, recHigh];
  const memoryTicks = [1, Math.round(maxGB / 4), Math.round(maxGB / 2), Math.round(maxGB * 0.75), maxGB].filter(
    (value, index, values) => value >= 1 && value <= maxGB && values.indexOf(value) === index,
  );

  useEffect(() => {
    setUsername(savedUsername);
    setMemGB(savedMemGB);
    setDiscordRpcEnabled(savedDiscordRpc);
  }, [savedDiscordRpc, savedMemGB, savedUsername]);

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

  const toggleDiscordRpc = async (): Promise<void> => {
    if (savingDiscordRpc) return;
    const next = !discordRpcEnabled;
    setDiscordRpcEnabled(next);
    setSavingDiscordRpc(true);
    try {
      const res: any = await api('PUT', '/config', {
        discord_rpc_enabled: next,
        discord_rpc_onboarding_seen: true,
      });
      if (res?.error) throw new Error(res.error);
      config.value = res;
      toast('Saved');
    } catch (err) {
      setDiscordRpcEnabled(savedDiscordRpc);
      toast(`Could not save Discord activity setting: ${errMessage(err)}`, 'error');
    } finally {
      setSavingDiscordRpc(false);
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
          {dirty && (
            <Button
              size="sm"
              onClick={() => {
                void save();
              }}
              disabled={!nameValid}
              sound="affirm"
            >
              Save
            </Button>
          )}
          {showNameError && <span class="cp-settings-name-err">{nameError}</span>}
        </div>
      </SettingsCard>
      <SettingsCard
        title="Memory"
        desc={`Maximum RAM given to the JVM when launching. ${recText} (system has ${totalGB} GB).`}
        stack
      >
        <div class="cp-settings-slider-control">
          <div class="cp-settings-readout">
            <span class="cp-settings-readout-label">Allocation</span>
            <span class="cp-settings-readout-value">{fmtMem(memGB)}</span>
          </div>
          <Slider
            value={memGB}
            min={1}
            max={maxGB}
            step={0.5}
            recommended={recZone}
            ticks={memoryTicks}
            sound="memory"
            onChange={setMemGB}
            onCommit={(next) => {
              void save(next);
            }}
            ariaLabel="Max memory in gigabytes"
          />
        </div>
      </SettingsCard>
      <SettingsCard
        title="Discord activity"
        desc="Shows Croopor and broad Minecraft status on your Discord profile."
        control={<Toggle on={discordRpcEnabled} onChange={() => void toggleDiscordRpc()} />}
      />
    </>
  );
}

const PERFORMANCE_OPTIONS: Array<ModeOption<PerformanceMode>> = [
  {
    value: 'managed',
    label: 'Managed',
    icon: 'sparkles',
    note: 'Croopor applies recommended tuning and optimizations for you.',
  },
  { value: 'vanilla', label: 'Vanilla', icon: 'cube', note: 'Pure Minecraft. No tweaks or add-ons applied at launch.' },
  {
    value: 'custom',
    label: 'Custom',
    icon: 'sliders',
    note: 'You set the tuning. Your manual choices are kept as-is.',
  },
];

const GUARDIAN_OPTIONS: Array<ModeOption<GuardianMode>> = [
  {
    value: 'managed',
    label: 'Managed',
    icon: 'shield-check',
    note: 'Catches risky launch settings and fixes them automatically.',
  },
  {
    value: 'custom',
    label: 'Custom',
    icon: 'shield-person',
    note: 'Keeps your choices, warns instead of changing, blocks only fatal setups.',
  },
];

type RulesStatusState =
  | { status: 'loading'; data: null; error?: undefined }
  | { status: 'ready'; data: PerformanceRulesStatus; error?: undefined }
  | { status: 'error'; data: null; error: string };

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
  const viewModel = status.view_model;

  return (
    <div class={className} aria-live="polite">
      <div class="cp-settings-rule-status-head">
        <div class="cp-settings-rule-status-copy">
          <strong>{viewModel.source_label} active</strong>
          <span>{viewModel.summary}</span>
        </div>
        <Pill tone={viewModel.validation_tone} icon={viewModel.validation_icon}>
          {viewModel.validation_label}
        </Pill>
      </div>
      <div class="cp-settings-rule-status-meta">
        <span>Source</span>
        <strong>{viewModel.channel_label}</strong>
        <span>Refresh</span>
        <strong>{viewModel.refresh_label}</strong>
        <span>Compositions</span>
        <strong>{status.composition_count}</strong>
      </div>
      <details class="cp-settings-rule-details">
        <summary>{viewModel.details_label}</summary>
        {viewModel.warnings.length > 0 && (
          <div class="cp-settings-rule-status-warnings">
            {viewModel.warnings.map((warning) => (
              <span key={warning}>{warning}</span>
            ))}
          </div>
        )}
        <div class="cp-settings-rule-status-grid">
          <span>Schema</span>
          <strong>v{status.schema_version}</strong>
          <span>Generated</span>
          <strong>{viewModel.generated_label}</strong>
          <span>Rules cache</span>
          <strong>{viewModel.cache_label}</strong>
          <span>Emergency disables</span>
          <strong>{viewModel.emergency_disable_label}</strong>
          <span>Bundle health</span>
          <strong>{viewModel.health_states_label}</strong>
          <span>Ownership</span>
          <strong>{viewModel.ownership_label}</strong>
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
    return () => {
      alive = false;
    };
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
      <SettingsCard title="Performance" desc="Launch behavior and managed rule readiness." stack>
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

function AudioSection(): JSX.Element {
  musicStateVersion.value;
  const [soundsOn, setSoundsOn] = useState<boolean>(local.sounds);
  const [musicOn, setMusicOn] = useState<boolean>(Music.enabled);
  const [volume, setVolume] = useState<number>(Music.volume);

  useEffect(() => {
    setMusicOn(Music.enabled);
    setVolume(Music.volume);
  }, [musicStateVersion.value]);

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
          <div class="cp-settings-slider-control">
            <div class="cp-settings-readout">
              <span class="cp-settings-readout-label">Volume</span>
              <span class="cp-settings-readout-value">{volume}%</span>
            </div>
            <Slider
              value={volume}
              min={0}
              max={100}
              step={1}
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
      <div class="cp-settings-shortcuts-list">
        {rows.map(([label, combo]) => (
          <div key={label} class="cp-settings-shortcut-row">
            <span class="cp-settings-shortcut-label">{label}</span>
            <kbd class="cp-kbd">{combo}</kbd>
          </div>
        ))}
      </div>
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
        {section === 'gameplay' && <GameplaySection />}
        {section === 'performance' && <PerformanceSection />}
        {section === 'audio' && <AudioSection />}
        {section === 'shortcuts' && <ShortcutsSection />}
        {section === 'advanced' && <AdvancedSettingsSection />}
        {section === 'about' && <AboutSettingsSection />}
      </div>
    </div>
  );
}
