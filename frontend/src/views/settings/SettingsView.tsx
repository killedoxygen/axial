import type { JSX, ComponentChildren } from 'preact';
import { useEffect, useMemo, useState } from 'preact/hooks';
import { Button, Input } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { Slider } from '../../ui/Slider';
import { AccentField, AccentModeToggle } from './AccentEditor';
import { local, saveLocalState } from '../../state';
import { Sound, playSliderSound } from '../../sound';
import { Music, musicStateVersion } from '../../music';
import { config, systemInfo, devMode, appVersion } from '../../store';
import { api } from '../../api';
import { toast } from '../../toast';
import { clampPlayerNameInput } from '../../player-name';
import { errMessage, fmtMem, getMemoryRecommendation, validateUsername } from '../../utils';
import './settings.css';

type SectionId = 'appearance' | 'gameplay' | 'audio' | 'shortcuts' | 'advanced' | 'about';

const SECTIONS: Array<{ id: SectionId; label: string; icon: string }> = [
  { id: 'appearance', label: 'Appearance', icon: 'palette' },
  { id: 'gameplay',   label: 'Gameplay',   icon: 'cube' },
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
    <div class={`cp-settings-card${stack ? ' cp-settings-card--stack' : ''}`}>
      <div>
        <div class="cp-settings-card-title">{title}</div>
        {desc && <div class="cp-settings-card-desc">{desc}</div>}
        {stack && children}
      </div>
      {(control || (!stack && children)) && (
        <div class="cp-settings-card-control">{control || children}</div>
      )}
    </div>
  );
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

// ── Appearance ─────────────────────────────────────────────────────────

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

// ── Gameplay ────────────────────────────────────────────────────────────

function GameplaySection(): JSX.Element {
  const cfg = config.value;
  const sys = systemInfo.value;
  const savedUsername = cfg?.username || 'Player';
  const savedMemGB = (cfg?.max_memory_mb ?? 4096) / 1024;
  const [username, setUsername] = useState(cfg?.username || 'Player');
  const [memGB, setMemGB] = useState<number>(savedMemGB);
  const totalGB = sys?.total_memory_mb ? Math.floor(sys.total_memory_mb / 1024) : 16;
  const maxGB = Math.max(1, totalGB);
  const rec = getMemoryRecommendation(totalGB);
  const recZone: [number, number] = [Math.max(2, rec.rec - 2), Math.min(maxGB, rec.rec + 2)];

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

  const save = async (): Promise<void> => {
    if (!dirty || !nameValid) return;
    try {
      const res: any = await api('PUT', '/config', {
        username: username.trim(),
        max_memory_mb: Math.round(memGB * 1024),
      });
      if (res.error) throw new Error(res.error);
      config.value = res;
      toast('Saved');
    } catch (err) {
      toast(`Failed: ${errMessage(err)}`);
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
          {dirty && <Button size="sm" onClick={save} disabled={!nameValid}>Save</Button>}
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
            ticks={[1, Math.round(maxGB / 4), Math.round(maxGB / 2), Math.round(maxGB * 0.75), maxGB].filter((v, i, arr) => arr.indexOf(v) === i)}
            onChange={(v) => {
              setMemGB(v);
              playSliderSound(v / maxGB, 'memory');
            }}
            onCommit={() => { if (dirty) void save(); }}
            ariaLabel="Max memory in gigabytes"
          />
        </div>
      </SettingsCard>
    </>
  );
}

// ── Audio ────────────────────────────────────────────────────────────

function AudioSection(): JSX.Element {
  // Reactive subscription to Music state
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
              onChange={(v) => {
                setVolume(v);
                Music.setVolume(v);
                playSliderSound(v / 100, 'volume');
              }}
              ariaLabel="Music volume"
            />
          </div>
        </SettingsCard>
      )}
    </>
  );
}

// ── Shortcuts ────────────────────────────────────────────────────────────

function ShortcutsSection(): JSX.Element {
  const rows: Array<[string, string]> = [
    ['Open settings', 'Ctrl + ,'],
    ['Focus search', 'Ctrl + F'],
    ['New instance', 'Ctrl + N'],
    ['Launch selected', 'Ctrl + Enter'],
    ['Close dialogs', 'Esc'],
  ];
  return (
    <SettingsCard title="Keyboard shortcuts" desc="Global shortcuts built into the launcher. Custom rebinding is coming." stack>
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

// ── Advanced ────────────────────────────────────────────────────────────

function AdvancedSection(): JSX.Element {
  const isDev = devMode.value;
  const [busy, setBusy] = useState(false);

  const flush = async (): Promise<void> => {
    const { showConfirm } = await import('../../ui/Dialog');
    const ok = await showConfirm('Delete all Croopor-owned data and reset the launcher to first run?', { destructive: true, confirmText: 'Reset' });
    if (!ok) return;
    setBusy(true);
    try {
      await api('POST', '/dev/flush');
      localStorage.clear();
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
        title="Reload launcher"
        desc="Useful if the launcher gets out of sync with the backend."
        control={<Button variant="secondary" icon="refresh" onClick={() => location.reload()}>Reload</Button>}
      />
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

// ── About ──────────────────────────────────────────────────────────────

function AboutSection(): JSX.Element {
  return (
    <SettingsCard title="Croopor" desc={`Version ${appVersion.value}. A focused Minecraft launcher.`} stack>
      <div style={{ marginTop: 12, display: 'flex', gap: 8 }}>
        <Button variant="secondary" icon="globe" onClick={() => window.open('https://github.com/mateoltd/croopor', '_blank', 'noopener')}>Homepage</Button>
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
        {section === 'audio' && <AudioSection />}
        {section === 'shortcuts' && <ShortcutsSection />}
        {section === 'advanced' && <AdvancedSection />}
        {section === 'about' && <AboutSection />}
      </div>
    </div>
  );
}
