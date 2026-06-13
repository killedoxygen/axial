import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Input } from '../../ui/Atoms';
import { Slider } from '../../ui/Slider';
import { Icon } from '../../ui/Icons';
import { AccentField, AccentModeToggle } from '../settings/AccentEditor';
import { WindowControls } from '../../shell/WindowControls';
import { local } from '../../state';
import { Music } from '../../music';
import { Sound } from '../../sound';
import { api } from '../../api';
import { config, systemInfo } from '../../store';
import { showOnboardingOverlay } from '../../ui-state';
import { toast } from '../../toast';
import { clampPlayerNameInput } from '../../player-name';
import { errMessage, fmtMem, getMemoryRecommendation, validateUsername } from '../../utils';
import { authStatusResponse, isRecord } from '../accounts/api';
import {
  copyText,
  formatSeconds,
  statusCanSelectOnline,
  type AuthLoginPending,
} from '../accounts/auth';
import type { AuthStatusRecord, AuthStatusState } from '../accounts/types';
import { useMicrosoftDeviceLogin } from '../accounts/useMicrosoftDeviceLogin';

type Stage = 'name' | 'memory' | 'color' | 'music';
const ORDER: Stage[] = ['name', 'memory', 'color', 'music'];
const STAGE_LABELS: Record<Stage, string> = {
  name: 'Name',
  memory: 'Memory',
  color: 'Mood',
  music: 'Sound',
};

async function readAuthStatus(): Promise<AuthStatusRecord> {
  const response = await api('GET', '/auth/status');
  if (isRecord(response) && typeof response.error === 'string') {
    throw new Error(response.error);
  }
  const parsed = authStatusResponse(response);
  if (!parsed) throw new Error('invalid auth status');
  return parsed;
}

function MicrosoftMark(): JSX.Element {
  return (
    <svg class="cp-ob-msa-mark" width="16" height="16" viewBox="0 0 16 16" aria-hidden="true">
      <rect x="0" y="0" width="7" height="7" fill="#f25022" />
      <rect x="9" y="0" width="7" height="7" fill="#7fba00" />
      <rect x="0" y="9" width="7" height="7" fill="#00a4ef" />
      <rect x="9" y="9" width="7" height="7" fill="#ffb900" />
    </svg>
  );
}

function OnboardingDeviceCodePanel({
  login,
  pollHint,
  onCancel,
}: {
  login: AuthLoginPending;
  pollHint: string | null;
  onCancel: () => void;
}): JSX.Element {
  const [copied, setCopied] = useState<'code' | 'url' | null>(null);

  const copy = (target: 'code' | 'url', value: string): void => {
    void copyText(value)
      .then(() => setCopied(target))
      .catch(() => setCopied(null));
  };

  return (
    <div class="cp-ob-devicecode">
      <div class="cp-ob-devicecode-row">
        <span class="cp-ob-devicecode-code">{login.user_code}</span>
        <button
          class="cp-ob-devicecode-action"
          type="button"
          onClick={() => copy('code', login.user_code)}
        >
          {copied === 'code' ? 'Copied' : 'Copy'}
        </button>
        <button class="cp-ob-devicecode-action" type="button" onClick={onCancel}>
          Cancel
        </button>
      </div>
      <div class="cp-ob-devicecode-row">
        <a href={login.verification_uri} target="_blank" rel="noreferrer">
          {login.verification_uri}
        </a>
        <button
          class="cp-ob-devicecode-action"
          type="button"
          onClick={() => copy('url', login.verification_uri)}
        >
          {copied === 'url' ? 'Copied' : 'Copy link'}
        </button>
      </div>
      <div class="cp-ob-devicecode-meta">
        <span>Expires in {formatSeconds(login.expires_in)}</span>
        <span>{pollHint || 'Waiting for approval'}</span>
      </div>
    </div>
  );
}

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
  const [authStatus, setAuthStatus] = useState<AuthStatusRecord | null>(null);
  const [authState, setAuthState] = useState<AuthStatusState>('loading');
  const [onlineAfterOnboarding, setOnlineAfterOnboarding] = useState(false);
  const microsoftLogin = useMicrosoftDeviceLogin({
    canStart: !saving && authState === 'ready' && authStatus?.login_available !== false,
    onAuthenticated: async (poll) => {
      let refreshedStatus: AuthStatusRecord | null = null;
      try {
        refreshedStatus = await readAuthStatus();
      } catch {
        refreshedStatus = null;
      }

      if (refreshedStatus) {
        setAuthStatus(refreshedStatus);
        setAuthState('ready');
      }
      const profileName = poll.minecraft_profile?.name
        ?? refreshedStatus?.minecraft_profile?.name
        ?? refreshedStatus?.username
        ?? '';
      const nextUsername = clampPlayerNameInput(profileName);
      if (nextUsername) setUsername(nextUsername);

      const onlineReady = refreshedStatus ? statusCanSelectOnline(refreshedStatus) : false;
      setOnlineAfterOnboarding(onlineReady);
      if (onlineReady && nextUsername && validateUsername(nextUsername) === null && stage === 'name') {
        setStage('memory');
        setMaxReached((m) => Math.max(m, 1));
        Sound.ui('affirm');
      }
    },
  });

  const idx = ORDER.indexOf(stage);
  const nameError = validateUsername(username);
  const nameValid = nameError === null;
  // Only surface the error after the user has actually typed something;
  // don't shout at an empty input they haven't touched yet.
  const showNameError = username.length > 0 && !nameValid;

  // Single source of truth for whether the user can move off the current stage.
  // Every forward path (button, keyboard Enter) funnels through this.
  const canAdvance: boolean =
    stage === 'name'   ? nameValid && !microsoftLogin.busy && !microsoftLogin.login :
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

  useEffect(() => {
    let active = true;
    setAuthState('loading');
    void readAuthStatus()
      .then((status) => {
        if (!active) return;
        setAuthStatus(status);
        setOnlineAfterOnboarding(statusCanSelectOnline(status));
        setAuthState('ready');
      })
      .catch(() => {
        if (!active) return;
        setAuthStatus(null);
        setAuthState('unavailable');
      });

    return () => {
      active = false;
    };
  }, []);

  const commit = async (): Promise<void> => {
    if (saving) return;
    if (!nameValid) {
      setStage('name');
      return;
    }
    if (musicEnabled == null) return;
    setSaving(true);
    const prevConfig = config.value;
    try {
      const patch: Record<string, unknown> = {
        username: username.trim(),
        max_memory_mb: Math.round(memory * 1024),
        music_enabled: musicEnabled,
        music_volume: 5,
      };
      if (onlineAfterOnboarding) {
        patch.launch_auth_mode = 'online';
      }
      const r: any = await api('PUT', '/config', patch);
      if (r.error) throw new Error(r.error);
      config.value = r;
      const complete = async (): Promise<void> => {
        const res: any = await api('POST', '/onboarding/complete');
        if (res?.error) throw new Error(res.error);
      };
      try {
        await complete();
      } catch {
        await complete();
      }
      Music.applyConfig({ music_enabled: musicEnabled, music_volume: 5 });
      if (musicEnabled) void Music.play();
      setDissolving(true);
      window.setTimeout(() => { showOnboardingOverlay.value = false; }, 560);
    } catch (err) {
      config.value = prevConfig;
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
  }, [
    stage,
    nameValid,
    musicEnabled,
    saving,
    dissolving,
    idx,
    maxReached,
    microsoftLogin.busy,
    microsoftLogin.login,
  ]);

  let headline = '';
  let subline: JSX.Element | null = null;
  let widget: JSX.Element;

  if (stage === 'name') {
    const authSignedIn = Boolean(authStatus?.msa_authenticated || onlineAfterOnboarding);
    const authUnavailable = authState === 'unavailable' || authStatus?.login_available === false;
    const authDisabled = saving ||
      microsoftLogin.busy ||
      Boolean(microsoftLogin.login) ||
      authState === 'loading' ||
      authUnavailable ||
      authSignedIn;
    const authStateText = authSignedIn
      ? 'Signed in'
      : microsoftLogin.login
        ? 'Waiting'
        : microsoftLogin.busy
          ? 'Starting'
          : authState === 'loading'
            ? 'Checking'
            : authUnavailable
              ? 'Unavailable'
              : 'Sign in';
    const authProfileName = authStatus?.minecraft_profile?.name ?? authStatus?.username ?? '';
    const authLabel = authSignedIn && authProfileName
      ? `Signed in as ${authProfileName}`
      : 'Sign in with your Minecraft account';
    headline = 'What should Minecraft call you?';
    subline = (
      <p class="cp-ob-subline">
        Croopor starts offline-first. Pick the local player name this launcher should use on this device.
      </p>
    );
    widget = (
      <div class="cp-ob-widget cp-ob-namefield">
        {/* Error and hint share one reserved slot above the input.
            They are mutually exclusive (error only when the typed
            name is invalid; hint only when valid). Placing them
            above keeps the input and validation state visually grouped. */}
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
          onChange={(v) => setUsername(clampPlayerNameInput(v))}
          placeholder="Your name"
          autoFocus
          onFocus={() => setNameFocused(true)}
          onBlur={() => setNameFocused(false)}
        />
        <button
          class="cp-ob-msa"
          data-state={authSignedIn
            ? 'signed-in'
            : authUnavailable
              ? 'unavailable'
              : microsoftLogin.login
                ? 'pending'
                : 'ready'}
          disabled={authDisabled}
          type="button"
          title={authUnavailable
            ? authStatus?.login_reason ?? 'Microsoft sign-in is unavailable.'
            : 'Start Microsoft device-code sign-in'}
          onClick={() => void microsoftLogin.startLogin()}
        >
          <MicrosoftMark />
          <span class="cp-ob-msa-label">{authLabel}</span>
          <span class="cp-ob-msa-state">{authStateText}</span>
        </button>
        {microsoftLogin.login && (
          <OnboardingDeviceCodePanel
            login={microsoftLogin.login}
            pollHint={microsoftLogin.pollHint}
            onCancel={() => {
              microsoftLogin.cancelLogin();
            }}
          />
        )}
        {microsoftLogin.message && (
          <div class="cp-ob-auth-message" data-tone={microsoftLogin.message.tone}>
            {microsoftLogin.message.text}
          </div>
        )}
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
          sound="memory"
          onChange={setMemory}
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
