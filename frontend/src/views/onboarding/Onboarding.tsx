import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
import { Button, Input } from '../../ui/Atoms';
import { Slider } from '../../ui/Slider';
import { Icon } from '../../ui/Icons';
import { useTheme } from '../../hooks/use-theme';
import { ColorField } from '../settings/ColorField';
import { applyTheme } from '../../theme';
import { local, PRESET_HUES } from '../../state';
import { Music } from '../../music';
import { Sound, playSliderSound } from '../../sound';
import { api } from '../../api';
import { config, systemInfo } from '../../store';
import { showOnboardingOverlay } from '../../ui-state';
import { toast } from '../../toast';
import { errMessage, fmtMem, getMemoryRecommendation } from '../../utils';
import './onboarding.css';

type Step = 'welcome' | 'memory' | 'theme' | 'music' | 'done';

const ORDER: Step[] = ['welcome', 'memory', 'theme', 'music', 'done'];

export function Onboarding(): JSX.Element | null {
  const theme = useTheme();
  const [step, setStep] = useState<Step>('welcome');
  const [username, setUsername] = useState('Player');
  const totalGB = systemInfo.value?.total_memory_mb ? Math.floor(systemInfo.value.total_memory_mb / 1024) : 16;
  const rec = getMemoryRecommendation(totalGB);
  const [memory, setMemory] = useState<number>(rec.rec);
  const [hue, setHue] = useState<number>(local.customHue);
  const [vibrancy, setVibrancy] = useState<number>(local.customVibrancy);
  const [musicChoice, setMusicChoice] = useState<boolean>(true);
  const [saving, setSaving] = useState(false);

  if (!showOnboardingOverlay.value) return null;

  const idx = ORDER.indexOf(step);
  const isFirst = idx === 0;
  const isLast = idx === ORDER.length - 1;

  const advance = (): void => {
    const next = ORDER[Math.min(ORDER.length - 1, idx + 1)];
    setStep(next);
  };
  const back = (): void => {
    const prev = ORDER[Math.max(0, idx - 1)];
    setStep(prev);
    Sound.ui('soft');
  };

  const applyPreset = (id: string): void => {
    const h = PRESET_HUES[id];
    if (h == null) return;
    setHue(h);
    applyTheme(id, null, { silent: true, vibrancy, lightness: local.lightness });
  };

  const applyCustom = (h: number, v: number): void => {
    setHue(h);
    setVibrancy(v);
    applyTheme('custom', h, { silent: true, vibrancy: v, lightness: local.lightness });
  };

  const finish = async (): Promise<void> => {
    setSaving(true);
    try {
      const r: any = await api('PUT', '/config', {
        username: username.trim() || 'Player',
        max_memory_mb: Math.round(memory * 1024),
        music_enabled: musicChoice,
        music_volume: 5,
      });
      if (r.error) throw new Error(r.error);
      config.value = r;
      await api('POST', '/onboarding/complete');
      Music.applyConfig({ music_enabled: musicChoice, music_volume: 5 });
      if (musicChoice) void Music.play();
      showOnboardingOverlay.value = false;
    } catch (err) {
      toast(`Failed to finish onboarding: ${errMessage(err)}`);
    } finally {
      setSaving(false);
    }
  };

  return (
    <div class="cp-ob-overlay">
      <div class="cp-ob-card">
        {step === 'welcome' && (
          <div class="cp-ob-step">
            <div style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
              <img src="logo.svg" class="cp-logo" alt="" width="40" height="40" />
              <div>
                <h1 class="cp-ob-title">Welcome to Croopor</h1>
                <p class="cp-ob-sub" style={{ marginTop: 4 }}>A focused Minecraft launcher. Let's get you set up.</p>
              </div>
            </div>
            <div>
              <div style={{ fontSize: 12, fontWeight: 600, color: theme.n.textDim, marginBottom: 6 }}>Player name</div>
              <Input value={username} onChange={setUsername} placeholder="Player" autoFocus
                onKeyDown={(e) => { if (e.key === 'Enter' && username.trim()) advance(); }} />
              <div style={{ fontSize: 11, color: theme.n.textMute, marginTop: 6 }}>You can change this later in settings.</div>
            </div>
          </div>
        )}

        {step === 'memory' && (
          <div class="cp-ob-step">
            <h1 class="cp-ob-title">Memory allocation</h1>
            <p class="cp-ob-sub">Your system has about {totalGB} GB of RAM. We recommend {rec.rec} GB for Minecraft.</p>
            <div>
              <div style={{ display: 'flex', justifyContent: 'space-between', marginBottom: 6 }}>
                <span style={{ fontSize: 12, fontWeight: 600, color: theme.n.textDim }}>Max memory</span>
                <span style={{ fontSize: 12, fontWeight: 700, color: theme.n.text }}>{fmtMem(memory)}</span>
              </div>
              <Slider
                value={memory}
                min={1}
                max={totalGB}
                step={0.5}
                recommended={[Math.max(2, rec.rec - 2), Math.min(totalGB, rec.rec + 2)]}
                onChange={(v) => {
                  setMemory(v);
                  playSliderSound(v / totalGB, 'memory');
                }}
                ariaLabel="Max memory in gigabytes"
              />
              <div style={{ fontSize: 11, color: theme.n.textMute, marginTop: 4 }}>
                {memory < 2 ? 'low, may stutter' :
                  memory > totalGB * 0.75 ? 'leave some room for your OS' :
                  rec.text}
              </div>
            </div>
          </div>
        )}

        {step === 'theme' && (
          <div class="cp-ob-step">
            <h1 class="cp-ob-title">Pick your vibe</h1>
            <p class="cp-ob-sub">Pick a preset, then fine tune if you want. Every color you pick derives its own hover, tint, and contrast automatically.</p>
            <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap' }}>
              {Object.entries(PRESET_HUES).map(([id, h]) => {
                const active = local.theme === id;
                return (
                  <button
                    key={id}
                    onClick={() => applyPreset(id)}
                    style={{
                      display: 'inline-flex', alignItems: 'center', gap: 8,
                      padding: '6px 12px 6px 8px',
                      borderRadius: 999,
                      border: `1px solid ${active ? theme.accent.line : theme.n.line}`,
                      background: active ? theme.accent.softer : theme.n.surface2,
                      color: active ? theme.accent.base : theme.n.text,
                      fontSize: 12, fontWeight: 600,
                      cursor: 'pointer',
                      textTransform: 'capitalize',
                    }}
                  >
                    <span style={{ width: 14, height: 14, borderRadius: 999, background: `oklch(0.78 0.14 ${h})` }} />
                    {id}
                  </button>
                );
              })}
            </div>
            <ColorField hue={hue} vibrancy={vibrancy} onChange={applyCustom} onEnd={() => Sound.ui('theme')} />
          </div>
        )}

        {step === 'music' && (
          <div class="cp-ob-step">
            <h1 class="cp-ob-title">Background music</h1>
            <p class="cp-ob-sub">A relaxed ambient track while you're in the launcher. Pauses automatically when the game is running.</p>
            <div style={{ display: 'flex', gap: 10 }}>
              <button
                class="cp-ob-choice"
                data-active={musicChoice === true}
                onClick={() => { setMusicChoice(true); Sound.ui('affirm'); }}
                style={{ flex: 1 }}
              >
                <div class="cp-ob-choice-title">
                  <Icon name="music" size={16} style={{ display: 'inline', verticalAlign: '-2px', marginRight: 6 }} />
                  Enable music
                </div>
                <div class="cp-ob-choice-sub">Downloaded on first play (~12 MB), then works offline.</div>
              </button>
              <button
                class="cp-ob-choice"
                data-active={musicChoice === false}
                onClick={() => { setMusicChoice(false); Sound.ui('soft'); }}
                style={{ flex: 1 }}
              >
                <div class="cp-ob-choice-title">
                  <Icon name="music-off" size={16} style={{ display: 'inline', verticalAlign: '-2px', marginRight: 6 }} />
                  Silent launcher
                </div>
                <div class="cp-ob-choice-sub">No ambient audio in the launcher.</div>
              </button>
            </div>
          </div>
        )}

        {step === 'done' && (
          <div class="cp-ob-step">
            <div style={{
              width: 72, height: 72, borderRadius: '50%',
              background: theme.accent.soft,
              display: 'flex', alignItems: 'center', justifyContent: 'center',
              color: theme.accent.base,
            }}>
              <Icon name="check" size={36} stroke={2.4} />
            </div>
            <h1 class="cp-ob-title">You're all set</h1>
            <p class="cp-ob-sub">
              Head to <strong>New instance</strong> to create your first Minecraft setup, or explore
              Settings to tune performance and appearance further.
            </p>
          </div>
        )}

        <div class="cp-ob-dots">
          {ORDER.map(s => <span key={s} class="cp-ob-dot" data-active={s === step} />)}
        </div>

        <div class="cp-ob-footer">
          {!isFirst && <Button variant="ghost" icon="chevron-left" onClick={back}>Back</Button>}
          <div style={{ flex: 1 }} />
          {isLast ? (
            <Button icon="check" onClick={finish} disabled={saving}>{saving ? 'Saving…' : 'Finish'}</Button>
          ) : (
            <Button
              onClick={() => { Sound.ui('click'); advance(); }}
              disabled={step === 'welcome' && !username.trim()}
            >Continue</Button>
          )}
        </div>
      </div>
    </div>
  );
}
