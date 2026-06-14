import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Input } from '../../ui/Atoms';
import { Slider } from '../../ui/Slider';
import { Icon } from '../../ui/Icons';
import { MicrosoftMark } from '../../ui/MicrosoftMark';
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
import { refreshAccountSkin } from '../../player-skin';
import { errMessage, fmtMem, getMemoryRecommendation, validateUsername } from '../../utils';
import { hasNativeDesktopRuntime } from '../../native';
import { authStatusResponse, isRecord } from '../accounts/api';
import { statusCanSelectOnline } from '../accounts/auth';
import type { AuthStatusRecord, AuthStatusState } from '../accounts/types';
import { useMicrosoftSignIn } from '../accounts/useMicrosoftSignIn';

type Stage = 'name' | 'memory' | 'color' | 'music' | 'discord';
const ORDER: Stage[] = ['name', 'memory', 'color', 'music', 'discord'];
const STAGE_LABELS: Record<Stage, string> = {
  name: 'Name',
  memory: 'Memory',
  color: 'Mood',
  music: 'Sound',
  discord: 'Activity',
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
  current,
  maxReached,
  order,
  onJump,
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
        <span key={`sep-${i}`} class="cp-ob-stepper-sep" aria-hidden="true">
          ›
        </span>,
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
  const totalGB = systemInfo.value?.total_memory_mb ? Math.floor(systemInfo.value.total_memory_mb / 1024) : 16;
  const rec = getMemoryRecommendation(totalGB);

  const [stage, setStage] = useState<Stage>('name');
  const [maxReached, setMaxReached] = useState<number>(0);
  const [username, setUsername] = useState('');
  const [memory, setMemory] = useState<number>(rec.rec);
  const [musicEnabled, setMusicEnabled] = useState<boolean | null>(null);
  const [discordRpcEnabled, setDiscordRpcEnabled] = useState<boolean>(config.value?.discord_rpc_enabled !== false);
  const [isWeirdo, setIsWeirdo] = useState<boolean>(local.lightness >= 50);
  const [saving, setSaving] = useState(false);
  const [dissolving, setDissolving] = useState(false);
  // The focused input hint shows Enter because arrow keys move the text cursor.
  const [nameFocused, setNameFocused] = useState(false);
  const [authStatus, setAuthStatus] = useState<AuthStatusRecord | null>(null);
  const [authState, setAuthState] = useState<AuthStatusState>('loading');
  const [onlineAfterOnboarding, setOnlineAfterOnboarding] = useState(false);
  const microsoftSignInAvailable = hasNativeDesktopRuntime() || authStatus?.login_available !== false;
  const microsoftLogin = useMicrosoftSignIn({
    canStart: !saving && authState === 'ready' && microsoftSignInAvailable,
    onAuthenticated: async (result) => {
      let refreshedStatus: AuthStatusRecord | null = null;
      try {
        refreshedStatus = await readAuthStatus();
      } catch (err: unknown) {
        console.warn('Could not refresh Microsoft sign-in status during onboarding.', err);
        refreshedStatus = null;
      }

      if (refreshedStatus) {
        setAuthStatus(refreshedStatus);
        setAuthState('ready');
      }
      const profileName =
        refreshedStatus?.minecraft_profile?.name ?? result.profile_name ?? refreshedStatus?.username ?? '';
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
  const showNameError = username.length > 0 && !nameValid;

  const canAdvance: boolean =
    stage === 'name'
      ? nameValid && !microsoftLogin.busy
      : stage === 'memory'
        ? true
        : stage === 'color'
          ? true
          : stage === 'music'
            ? !saving && musicEnabled != null
            : !saving;

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
    // Only stages reached through the validated path can be jump targets.
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
        max_memory_mb: Math.round(memory * 1024),
        music_enabled: musicEnabled,
        music_volume: 5,
        discord_rpc_enabled: discordRpcEnabled,
        discord_rpc_onboarding_seen: true,
      };
      if (onlineAfterOnboarding) {
        patch.launch_auth_mode = 'online';
      } else {
        const account = await api('POST', '/accounts/offline', { username: username.trim() });
        if (isRecord(account) && typeof account.error === 'string') {
          throw new Error(account.error);
        }
        patch.username = username.trim();
        patch.launch_auth_mode = 'offline';
      }
      const r: any = await api('PUT', '/config', patch);
      if (r.error) throw new Error(r.error);
      config.value = r;
      if (patch.launch_auth_mode === 'online') {
        try {
          await api('POST', '/skins/from-profile', { mark_current: true });
        } catch (err: unknown) {
          console.warn('Could not seed profile skin after onboarding Microsoft sign-in.', err);
        }
      }
      refreshAccountSkin();
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
      window.setTimeout(() => {
        showOnboardingOverlay.value = false;
      }, 560);
    } catch (err) {
      config.value = prevConfig;
      toast(`Couldn't finish onboarding: ${errMessage(err)}`);
      setSaving(false);
    }
  };

  const onCta = (): void => {
    if (!canAdvance) return;
    if (stage === 'discord') void commit();
    else advance();
  };

  // Enter always advances; arrow keys keep native cursor behavior in text fields.
  useEffect(() => {
    const h = (e: KeyboardEvent): void => {
      if (dissolving || saving) return;

      const target = e.target as HTMLElement | null;
      const inTextField = target != null && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA');

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
    return () => {
      window.removeEventListener('keydown', h);
    };
  }, [stage, nameValid, musicEnabled, discordRpcEnabled, saving, dissolving, idx, maxReached, microsoftLogin.busy]);

  let headline = '';
  let subline: JSX.Element | null = null;
  let widget: JSX.Element;

  if (stage === 'name') {
    const authSignedIn = Boolean(authStatus?.msa_authenticated || onlineAfterOnboarding);
    const authUnavailable = authState === 'unavailable' || !microsoftSignInAvailable;
    const authDisabled = saving || microsoftLogin.busy || authState === 'loading' || authUnavailable || authSignedIn;
    const authStateText = authSignedIn
      ? 'Signed in'
      : microsoftLogin.busy
        ? 'Starting'
        : authState === 'loading'
          ? 'Checking'
          : authUnavailable
            ? 'Unavailable'
            : 'Sign in';
    const authProfileName = authStatus?.minecraft_profile?.name ?? authStatus?.username ?? '';
    const authLabel =
      authSignedIn && authProfileName ? `Signed in as ${authProfileName}` : 'Sign in with your Minecraft account';
    headline = 'What should Minecraft call you?';
    widget = (
      <div class="cp-ob-widget cp-ob-namefield">
        <div class="cp-ob-feedback" data-state={showNameError ? 'error' : nameValid ? 'hint' : 'idle'}>
          {showNameError ? (
            <span class="cp-ob-feedback-error">{nameError}</span>
          ) : nameValid ? (
            <span class="cp-ob-feedback-hint">
              Press <kbd>Enter</kbd>
              {!nameFocused && (
                <>
                  {' '}
                  or <kbd>→</kbd>
                </>
              )}{' '}
              to continue
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
          data-state={
            authSignedIn ? 'signed-in' : authUnavailable ? 'unavailable' : microsoftLogin.busy ? 'pending' : 'ready'
          }
          disabled={authDisabled}
          type="button"
          title={
            authUnavailable
              ? (authStatus?.login_reason ?? 'Microsoft sign-in is unavailable.')
              : 'Sign in with Microsoft'
          }
          onClick={() => void microsoftLogin.startLogin()}
        >
          <MicrosoftMark class="cp-ob-msa-mark" />
          <span class="cp-ob-msa-label">{authLabel}</span>
          <span class="cp-ob-msa-state">{authStateText}</span>
        </button>
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
          {isWeirdo && (
            <span class="cp-ob-weirdo" aria-hidden="true">
              Weirdo!
            </span>
          )}
        </div>
        <AccentField showPresets={false} />
      </div>
    );
  } else if (stage === 'music') {
    headline = 'Quiet, or a little atmosphere?';
    subline = <p class="cp-ob-subline">Ambient music pauses itself when the game starts.</p>;
    widget = (
      <div class="cp-ob-widget">
        <div class="cp-ob-pills">
          <button
            class="cp-ob-pill"
            data-active={musicEnabled === true}
            onClick={() => {
              setMusicEnabled(true);
              Sound.ui('affirm');
            }}
            type="button"
          >
            <Icon name="music" size={16} />
            <span>Ambient music</span>
          </button>
          <button
            class="cp-ob-pill"
            data-active={musicEnabled === false}
            onClick={() => {
              setMusicEnabled(false);
              Sound.ui('soft');
            }}
            type="button"
          >
            <Icon name="music-off" size={16} />
            <span>Silent launcher</span>
          </button>
        </div>
      </div>
    );
  } else {
    headline = 'Share launcher activity on Discord?';
    subline = (
      <p class="cp-ob-subline">Croopor shares broad Minecraft activity, not instance names or server details.</p>
    );
    widget = (
      <div class="cp-ob-widget">
        <div class="cp-ob-pills">
          <button
            class="cp-ob-pill"
            data-active={discordRpcEnabled === true}
            onClick={() => {
              setDiscordRpcEnabled(true);
              Sound.ui('affirm');
            }}
            type="button"
          >
            <Icon name="activity" size={16} />
            <span>Discord activity</span>
          </button>
          <button
            class="cp-ob-pill"
            data-active={discordRpcEnabled === false}
            onClick={() => {
              setDiscordRpcEnabled(false);
              Sound.ui('soft');
            }}
            type="button"
          >
            <Icon name="shield-check" size={16} />
            <span>Private launcher</span>
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
            <h1 class="cp-ob-headline">
              <Words text={headline} />
            </h1>
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
            aria-label={stage === 'discord' ? 'Finish (Enter)' : 'Next step (Right arrow)'}
            title={stage === 'discord' ? (saving ? 'Starting…' : 'Finish  ↵') : 'Next  →'}
          >
            <Icon name="chevron-right" size={18} stroke={2.2} />
          </button>
        </div>
        <div class="cp-ob-footnote">You can change any of this later in Settings.</div>
      </footer>
    </div>
  );
}
