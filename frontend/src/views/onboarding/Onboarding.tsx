import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Input } from '../../ui/Atoms';
import { Slider } from '../../ui/Slider';
import { Icon } from '../../ui/Icons';
import { AccentField, AccentModeToggle } from '../settings/AccentEditor';
import { WindowControls } from '../../shell/WindowControls';
import { local } from '../../state';
import { Music } from '../../music';
import { Sound, playSliderSound } from '../../sound';
import { api } from '../../api';
import { config, systemInfo } from '../../store';
import { showOnboardingOverlay } from '../../ui-state';
import { toast } from '../../toast';
import { errMessage, fmtMem, getMemoryRecommendation, USERNAME_MAX_LEN, validateUsername } from '../../utils';
import './onboarding.css';

type Stage = 'name' | 'memory' | 'color' | 'music';
const ORDER: Stage[] = ['name', 'memory', 'color', 'music'];
const STAGE_LABELS: Record<Stage, string> = {
  name: 'Name',
  memory: 'Memory',
  color: 'Mood',
  music: 'Sound',
};

function Words({ text }: { text: string }): JSX.Element {
  // Keep the space as a sibling text node between word spans, not inside them.
  // Trailing whitespace inside an inline-block gets swallowed by layout, which
  // collapsed the headline into "WhatshouldMinecraftcallyou?". Sibling text nodes
  // also preserve natural line-break opportunities between words.
  const parts = text.split(' ');
  return (
    <>
      {parts.flatMap((w, i) => {
        const span = (
          <span key={`w${i}`} class="cp-ob-word" style={{ ['--i' as any]: String(i) }}>
            {w}
          </span>
        );
        return i === 0 ? [span] : [' ', span];
      })}
    </>
  );
}

function Stepper({
  current, maxReached, order, onJump,
}: {
  current: number;
  maxReached: number;
  order: Stage[];
  onJump: (i: number) => void;
}): JSX.Element {
  const nodes: JSX.Element[] = [];
  order.forEach((s, i) => {
    if (i > 0) {
      nodes.push(
        <span key={`sep-${i}`} class="cp-ob-stepper-sep" aria-hidden="true">›</span>,
      );
    }
    const state = i < current ? 'past' : i === current ? 'active' : 'future';
    const clickable = i !== current && i <= maxReached;
    const label = STAGE_LABELS[s];
    const num = String(i + 1).padStart(2, '0');
    const inner = (
      <>
        <span class="cp-ob-stepper-num">{num}</span>
        <span class="cp-ob-stepper-label">{label}</span>
      </>
    );
    if (clickable) {
      nodes.push(
        <button
          key={s}
          type="button"
          class="cp-ob-stepper-item"
          data-state={state}
          onClick={() => onJump(i)}
          aria-label={`Go to ${label}`}
        >
          {inner}
        </button>,
      );
    } else {
      nodes.push(
        <div
          key={s}
          class="cp-ob-stepper-item"
          data-state={state}
          aria-current={state === 'active' ? 'step' : undefined}
        >
          {inner}
        </div>,
      );
    }
  });
  return (
    <nav class="cp-ob-stepper" aria-label="Onboarding progress">
      {nodes}
    </nav>
  );
}

export function Onboarding(): JSX.Element | null {
  const totalGB = systemInfo.value?.total_memory_mb
    ? Math.floor(systemInfo.value.total_memory_mb / 1024)
    : 16;
  const rec = getMemoryRecommendation(totalGB);

  const [stage, setStage] = useState<Stage>('name');
  const [maxReached, setMaxReached] = useState<number>(0);
  const [username, setUsername] = useState('');
  const [memory, setMemory] = useState<number>(rec.rec);
  const [musicEnabled, setMusicEnabled] = useState<boolean | null>(null);
  const [isWeirdo, setIsWeirdo] = useState<boolean>(local.lightness >= 50);
  const [saving, setSaving] = useState(false);
  const [dissolving, setDissolving] = useState(false);
  // Track name-input focus so the keyboard hint can mention the right key:
  // Enter works from inside the input; → does NOT (arrow keys move the cursor
  // there). Showing → while focused would mislead the user.
  const [nameFocused, setNameFocused] = useState(false);

  const idx = ORDER.indexOf(stage);
  const nameError = validateUsername(username);
  const nameValid = nameError === null;
  // Only surface the error after the user has actually typed something;
  // don't shout at an empty input they haven't touched yet.
  const showNameError = username.length > 0 && !nameValid;

  // Single source of truth for whether the user can move off the current stage.
  // Every forward path (button, keyboard Enter) funnels through this.
  const canAdvance: boolean =
    stage === 'name'   ? nameValid :
    stage === 'memory' ? true :
    stage === 'color'  ? true :
    stage === 'music'  ? (!saving && musicEnabled != null) :
    false;

  const advance = (): void => {
    const nextIdx = idx + 1;
    if (nextIdx >= ORDER.length) return;
    setStage(ORDER[nextIdx] as Stage);
    setMaxReached((m) => Math.max(m, nextIdx));
    Sound.ui('click');
  };

  const jumpTo = (i: number): void => {
    const target = ORDER[i];
    if (!target || i === idx) return;
    // Can't leap into a step that hasn't been reached yet — that has to be earned
    // by a forward commit through the validated path.
    if (i > maxReached) return;
    setStage(target);
    Sound.ui('soft');
  };

  const commit = async (): Promise<void> => {
    if (saving) return;
    if (!nameValid) {
      setStage('name');
      return;
    }
    if (musicEnabled == null) return;
    setSaving(true);
    try {
      const r: any = await api('PUT', '/config', {
        username: username.trim(),
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

  const onCta = (): void => {
    if (!canAdvance) return;
    if (stage === 'music') void commit();
    else advance();
  };

  // Window-level keyboard routing. One handler, three shortcuts:
  //   Enter       → advance / commit (same path as right arrow and ">")
  //   Arrow Right → advance / commit (same path as Enter and ">")
  //   Arrow Left  → back-nav to prior stage
  // Arrow keys inside a text input keep their native cursor behavior, so we
  // skip both arrow branches when the user is typing. Enter always routes
  // through onCta — if the stage isn't ready (invalid name, no music pill),
  // onCta is a no-op, so nothing bad happens.
  useEffect(() => {
    const h = (e: KeyboardEvent): void => {
      if (dissolving || saving) return;

      const target = e.target as HTMLElement | null;
      const inTextField = target != null &&
        (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA');

      if (e.key === 'Enter') {
        e.preventDefault();
        onCta();
      } else if (e.key === 'ArrowRight' && !inTextField) {
        e.preventDefault();
        onCta();
      } else if (e.key === 'ArrowLeft' && !inTextField) {
        if (idx > 0) {
          e.preventDefault();
          jumpTo(idx - 1);
        }
      }
    };
    window.addEventListener('keydown', h);
    return () => { window.removeEventListener('keydown', h); };
  }, [stage, nameValid, musicEnabled, saving, dissolving, idx, maxReached]);

  let headline = '';
  let subline: JSX.Element | null = null;
  let widget: JSX.Element;

  if (stage === 'name') {
    headline = 'What should Minecraft call you?';
    widget = (
      <div class="cp-ob-widget cp-ob-namefield">
        {/* Error and hint share one reserved slot above the input.
            They are mutually exclusive (error only when the typed
            name is invalid; hint only when valid). Placing them
            above keeps the input visually adjacent to the sign-in
            button below — tightens the name-field group. */}
        <div class="cp-ob-feedback" data-state={showNameError ? 'error' : nameValid ? 'hint' : 'idle'}>
          {showNameError ? (
            <span class="cp-ob-feedback-error">{nameError}</span>
          ) : nameValid ? (
            <span class="cp-ob-feedback-hint">
              Press <kbd>Enter</kbd>
              {!nameFocused && <> or <kbd>→</kbd></>}
              {' '}to continue
            </span>
          ) : null}
        </div>
        <Input
          value={username}
          onChange={(v) => setUsername(v.slice(0, USERNAME_MAX_LEN))}
          placeholder="Your name"
          autoFocus
          onFocus={() => setNameFocused(true)}
          onBlur={() => setNameFocused(false)}
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
    );
  } else if (stage === 'memory') {
    headline = 'How much memory can Minecraft borrow?';
    subline = (
      <p class="cp-ob-subline">
        Your system has about {totalGB} GB. {rec.rec} GB is a comfortable starting point.
      </p>
    );
    widget = (
      <div class="cp-ob-widget cp-ob-memory">
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
        <div class="cp-ob-memory-summary" aria-live="polite">
          <div class="cp-ob-memreading">{fmtMem(memory)}</div>
          <div class="cp-ob-memory-note">Comfortable start: {rec.rec} GB</div>
        </div>
      </div>
    );
  } else if (stage === 'color') {
    headline = 'Pick a mood.';
    subline = (
      <p class="cp-ob-subline">Drag anywhere, tap a preset, or flip the canvas. Everything learns your color.</p>
    );
    widget = (
      <div class="cp-ob-widget">
        <div class="cp-ob-mode-row">
          <AccentModeToggle onChange={(m) => setIsWeirdo(m === 'light')} />
          {isWeirdo && <span class="cp-ob-weirdo" aria-hidden="true">Weirdo!</span>}
        </div>
        <AccentField showPresets={false} />
      </div>
    );
  } else {
    headline = 'Quiet, or a little atmosphere?';
    subline = <p class="cp-ob-subline">Ambient music pauses itself when the game starts.</p>;
    widget = (
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
    );
  }

  return (
    <div class={`cp-ob-root${dissolving ? ' is-dissolving' : ''}`}>
      <div class="cp-ob-topstrip cp-drag">
        <WindowControls />
      </div>

      <div class="cp-ob-statusbar">
        <Stepper current={idx} maxReached={maxReached} order={ORDER} onJump={jumpTo} />
      </div>

      <main class="cp-ob-main">
        <div class="cp-ob-column">
          <div class="cp-ob-stage" key={stage}>
            <h1 class="cp-ob-headline"><Words text={headline} /></h1>
            {subline}
            {widget}
          </div>
        </div>
      </main>

      <footer class="cp-ob-bottom">
        <div class="cp-ob-nav" role="group" aria-label="Step navigation">
          <button
            type="button"
            class="cp-ob-navbtn"
            onClick={() => jumpTo(idx - 1)}
            disabled={idx === 0 || saving}
            aria-label="Previous step (Left arrow)"
            title="Previous  ←"
          >
            <Icon name="chevron-left" size={18} stroke={2.2} />
          </button>
          <button
            type="button"
            class="cp-ob-navbtn"
            onClick={onCta}
            disabled={!canAdvance}
            aria-label={stage === 'music' ? 'Finish (Enter)' : 'Next step (Right arrow)'}
            title={stage === 'music' ? (saving ? 'Starting…' : 'Finish  ↵') : 'Next  →'}
          >
            <Icon name="chevron-right" size={18} stroke={2.2} />
          </button>
        </div>
        <div class="cp-ob-footnote">You can change any of this later in Settings.</div>
      </footer>
    </div>
  );
}
