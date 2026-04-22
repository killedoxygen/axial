import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
import { Button, Input } from '../../ui/Atoms';
import { Slider } from '../../ui/Slider';
import { Icon } from '../../ui/Icons';
import { ColorField } from '../settings/ColorField';
import { applyTheme } from '../../theme';
import { local } from '../../state';
import { Music } from '../../music';
import { Sound, playSliderSound } from '../../sound';
import { api } from '../../api';
import { config, systemInfo } from '../../store';
import { showOnboardingOverlay } from '../../ui-state';
import { toast } from '../../toast';
import { errMessage, fmtMem, getMemoryRecommendation } from '../../utils';
import './onboarding.css';

type Stage = 'name' | 'memory' | 'color' | 'music';
const ORDER: Stage[] = ['name', 'memory', 'color', 'music'];

function Words({ text }: { text: string }): JSX.Element {
  const parts = text.split(' ');
  return (
    <>
      {parts.map((w, i) => (
        <span class="cp-ob-word" style={{ ['--i' as any]: String(i) }}>
          {w}
          {i < parts.length - 1 ? ' ' : ''}
        </span>
      ))}
    </>
  );
}

export function Onboarding(): JSX.Element | null {
  const totalGB = systemInfo.value?.total_memory_mb
    ? Math.floor(systemInfo.value.total_memory_mb / 1024)
    : 16;
  const rec = getMemoryRecommendation(totalGB);

  const [stage, setStage] = useState<Stage>('name');
  const [username, setUsername] = useState('');
  const [memory, setMemory] = useState<number>(rec.rec);
  const [hue, setHue] = useState<number>(local.customHue);
  const [vibrancy, setVibrancy] = useState<number>(local.customVibrancy);
  const [musicEnabled, setMusicEnabled] = useState<boolean | null>(null);
  const [saving, setSaving] = useState(false);
  const [dissolving, setDissolving] = useState(false);

  const idx = ORDER.indexOf(stage);
  const nameValid = username.trim().length > 0;

  const advance = (): void => {
    const next = ORDER[idx + 1];
    if (next) {
      setStage(next);
      Sound.ui('click');
    }
  };

  const applyCustom = (h: number, v: number): void => {
    setHue(h);
    setVibrancy(v);
    applyTheme('custom', h, { silent: true, vibrancy: v, lightness: local.lightness });
  };

  const commit = async (): Promise<void> => {
    if (saving || musicEnabled == null) return;
    setSaving(true);
    try {
      const r: any = await api('PUT', '/config', {
        username: username.trim() || 'Player',
        max_memory_mb: Math.round(memory * 1024),
        music_enabled: musicEnabled,
        music_volume: 5,
      });
      if (r.error) throw new Error(r.error);
      config.value = r;
      await api('POST', '/onboarding/complete');
      Music.applyConfig({ music_enabled: musicEnabled, music_volume: 5 });
      if (musicEnabled) void Music.play();
      setDissolving(true);
      window.setTimeout(() => { showOnboardingOverlay.value = false; }, 560);
    } catch (err) {
      toast(`Couldn't finish onboarding: ${errMessage(err)}`);
      setSaving(false);
    }
  };

  const recapChips: JSX.Element[] = [];
  if (idx > 0) recapChips.push(
    <span class="cp-ob-chip" key="name">{username.trim() || 'Player'}</span>,
  );
  if (idx > 1) recapChips.push(
    <span class="cp-ob-chip" key="mem">{fmtMem(memory)}</span>,
  );
  if (idx > 2) recapChips.push(
    <span
      class="cp-ob-chip cp-ob-chip--color"
      key="color"
      aria-label="Chosen color"
      style={{ ['--swatch' as any]: `oklch(0.78 0.14 ${hue})` }}
    />,
  );

  return (
    <div class={`cp-ob-root${dissolving ? ' is-dissolving' : ''}`}>
      <div class="cp-ob-column">
        {recapChips.length > 0 && <div class="cp-ob-recap">{recapChips}</div>}

        <div class="cp-ob-stage" key={stage}>
          {stage === 'name' && (
            <>
              <h1 class="cp-ob-headline"><Words text="What should we call you?" /></h1>
              <div class="cp-ob-widget cp-ob-namefield">
                <Input
                  value={username}
                  onChange={setUsername}
                  placeholder="Your name"
                  autoFocus
                  onKeyDown={(e) => { if (e.key === 'Enter' && nameValid) advance(); }}
                />
                {/* Placeholder slot for future Microsoft-auth sign-in. Disabled on purpose;
                    when MSA lands, this becomes the primary path and the input the fallback. */}
                <button class="cp-ob-msa" disabled type="button" aria-disabled="true" tabIndex={-1}>
                  <svg class="cp-ob-msa-mark" width="16" height="16" viewBox="0 0 16 16" aria-hidden="true">
                    <rect x="0" y="0" width="7" height="7" fill="#f25022" />
                    <rect x="9" y="0" width="7" height="7" fill="#7fba00" />
                    <rect x="0" y="9" width="7" height="7" fill="#00a4ef" />
                    <rect x="9" y="9" width="7" height="7" fill="#ffb900" />
                  </svg>
                  <span class="cp-ob-msa-label">Sign in with your Minecraft account</span>
                  <span class="cp-ob-msa-soon">Coming soon</span>
                </button>
              </div>
              <div class={`cp-ob-hint${nameValid ? ' is-visible' : ''}`}>
                Press <kbd>Enter</kbd> to continue
              </div>
            </>
          )}

          {stage === 'memory' && (
            <>
              <h1 class="cp-ob-headline"><Words text="How much memory can Minecraft borrow?" /></h1>
              <p class="cp-ob-subline">
                Your system has about {totalGB} GB. {rec.rec} GB is a comfortable starting point.
              </p>
              <div class="cp-ob-widget">
                <div class="cp-ob-memreading">{fmtMem(memory)}</div>
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
              </div>
              <div class="cp-ob-cta">
                <Button size="lg" onClick={advance}>Continue</Button>
              </div>
            </>
          )}

          {stage === 'color' && (
            <>
              <h1 class="cp-ob-headline"><Words text="Pick a mood." /></h1>
              <p class="cp-ob-subline">Drag anywhere. The whole launcher learns your color.</p>
              <div class="cp-ob-widget">
                <ColorField
                  hue={hue}
                  vibrancy={vibrancy}
                  onChange={applyCustom}
                  onEnd={() => Sound.ui('theme')}
                />
              </div>
              <div class="cp-ob-cta">
                <Button size="lg" onClick={advance}>Continue</Button>
              </div>
            </>
          )}

          {stage === 'music' && (
            <>
              <h1 class="cp-ob-headline"><Words text="Quiet, or a little atmosphere?" /></h1>
              <p class="cp-ob-subline">Ambient music pauses itself when the game starts.</p>
              <div class="cp-ob-widget">
                <div class="cp-ob-pills">
                  <button
                    class="cp-ob-pill"
                    data-active={musicEnabled === true}
                    onClick={() => { setMusicEnabled(true); Sound.ui('affirm'); }}
                    type="button"
                  >
                    <Icon name="music" size={16} />
                    <span>Ambient music</span>
                  </button>
                  <button
                    class="cp-ob-pill"
                    data-active={musicEnabled === false}
                    onClick={() => { setMusicEnabled(false); Sound.ui('soft'); }}
                    type="button"
                  >
                    <Icon name="music-off" size={16} />
                    <span>Silent launcher</span>
                  </button>
                </div>
              </div>
              <div class={`cp-ob-cta cp-ob-cta--gated${musicEnabled != null ? ' is-visible' : ''}`}>
                <Button size="lg" onClick={commit} disabled={saving || musicEnabled == null}>
                  {saving ? 'Starting…' : "Let's go"}
                </Button>
              </div>
            </>
          )}
        </div>
      </div>
    </div>
  );
}
